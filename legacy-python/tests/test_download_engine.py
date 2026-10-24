"""Tests for ``src.core.download_worker`` (engine) + one manager integration case."""

from __future__ import annotations

import dataclasses
import hashlib
import pathlib
import threading
import time

import pytest

from src.core.download_manager import DownloadManager
from src.core.download_worker import DownloadWorker
from src.core import filename_utils
from src.database.models import (
    TERMINAL_STATES,
    DownloadProgress,
    DownloadRecord,
    DownloadState,
    Settings,
)
from src.database.repository import DownloadRepository


def _make_record(url: str, dest_dir: pathlib.Path, filename: str) -> DownloadRecord:
    """A worker-ready record with the destination path pre-sanitised."""
    name = filename_utils.sanitize_filename(filename)
    destination = filename_utils.resolve_destination(str(dest_dir), name)
    return DownloadRecord(
        source_url=url,
        display_name=name,
        destination_path=str(destination),
    )


# -- full download ----------------------------------------------------------------


def test_full_download_completes(http_server, settings: Settings, tmp_path: pathlib.Path) -> None:
    # Arrange
    record = _make_record(
        http_server.url("/files/data.bin"), tmp_path / "dl", "data.bin"
    )
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.COMPLETED
    assert settled.downloaded_bytes == 2048
    final = pathlib.Path(settled.destination_path)
    assert final.read_bytes() == http_server.file_bytes("data.bin")
    assert not pathlib.Path(str(final) + ".part").exists()


# -- progress callbacks --------------------------------------------------------------


def test_progress_callbacks_throttled_and_monotonic(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange: 200 KB at 100 KB/s with an 8 KB buffer -> ~2 s of callbacks.
    data = b"x" * 200_000
    http_server.set_file("progress.bin", data)
    slow = dataclasses.replace(settings, bandwidth_limit_bps=100_000)
    record = _make_record(
        http_server.url("/files/progress.bin"), tmp_path / "dl", "progress.bin"
    )
    callbacks: list[DownloadProgress] = []
    worker = DownloadWorker(record, slow, on_progress=callbacks.append)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.COMPLETED
    assert len(callbacks) >= 3
    done = [cb.downloaded_bytes for cb in callbacks]
    assert done == sorted(done)  # non-decreasing
    assert callbacks[-1].downloaded_bytes == len(data)
    assert callbacks[-1].total_bytes == len(data)


# -- HTTP failure mapping ---------------------------------------------------------------


def test_status_404_fails_without_retries(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange
    record = _make_record(
        http_server.url("/status/404"), tmp_path / "dl", "missing.bin"
    )
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.FAILED
    assert settled.retry_count == 0
    assert settled.error_summary is not None
    assert "404" in settled.error_summary


@pytest.mark.parametrize("status", [401, 403])
def test_auth_required_status_maps_to_auth_error(
    http_server, settings: Settings, tmp_path: pathlib.Path, status: int
) -> None:
    # Arrange
    record = _make_record(
        http_server.url(f"/status/{status}"), tmp_path / "dl", "locked.bin"
    )
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.FAILED
    assert settled.error_summary is not None
    assert "authentication" in settled.error_summary.lower()


def test_html_response_refused(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange
    record = _make_record(
        http_server.url("/html-as-file"), tmp_path / "dl", "trap.bin"
    )
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.FAILED
    assert settled.error_summary is not None
    assert "HTML" in settled.error_summary
    final = pathlib.Path(settled.destination_path)
    assert not final.exists()


def test_content_length_mismatch_detected(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange
    record = _make_record(http_server.url("/mismatch"), tmp_path / "dl", "short.bin")
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.FAILED
    assert settled.error_summary is not None
    assert "mismatch" in settled.error_summary.lower()


# -- SHA-256 verification -----------------------------------------------------------


def test_checksum_correct_verifies(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange
    data = http_server.file_bytes("data.bin")
    digest = hashlib.sha256(data).hexdigest()
    record = _make_record(
        http_server.url("/files/data.bin"), tmp_path / "dl", "data.bin"
    )
    record.checksum_sha256 = digest
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.COMPLETED
    assert settled.checksum_verified is True


def test_checksum_wrong_fails(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange
    record = _make_record(
        http_server.url("/files/data.bin"), tmp_path / "dl", "data.bin"
    )
    record.checksum_sha256 = hashlib.sha256(b"not-the-content").hexdigest()
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.FAILED
    assert settled.checksum_verified is False
    final = pathlib.Path(settled.destination_path)
    assert not final.exists()  # corrupt file must not be published


def test_checksum_absent_completes_unverified(
    http_server, settings: Settings, tmp_path: pathlib.Path
) -> None:
    # Arrange
    record = _make_record(
        http_server.url("/files/data.bin"), tmp_path / "dl", "data.bin"
    )
    assert record.checksum_sha256 is None
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.COMPLETED
    assert settled.checksum_verified is False


# -- cooperative cancel ---------------------------------------------------------------


def test_cancel_mid_slow_stream(http_server, settings: Settings, tmp_path: pathlib.Path) -> None:
    # Arrange: small buffer so the dribbling /slow endpoint yields several
    # chunks over time; the first progress callback arms the cancel event.
    http_server.set_file("ignored.txt", b"z")  # keep site state stable
    trickle = dataclasses.replace(settings, buffer_size=32)
    record = _make_record(
        http_server.url("/slow"), tmp_path / "dl", "slow-item.bin"
    )
    cancel_event = threading.Event()

    def arm_cancel(progress: DownloadProgress) -> None:
        cancel_event.set()

    worker = DownloadWorker(record, trickle, on_progress=arm_cancel, cancel_event=cancel_event)

    # Act
    settled = worker.run()

    # Assert: cancelled mid-transfer, partial data preserved, nothing final.
    assert settled.state is DownloadState.CANCELLED
    part = pathlib.Path(settled.destination_path + ".part")
    assert part.is_file()
    assert part.stat().st_size > 0
    assert not pathlib.Path(settled.destination_path).exists()


# -- manager integration -----------------------------------------------------------------


def test_manager_completes_one_download(
    http_server, settings: Settings, tmp_path: pathlib.Path, db
) -> None:
    # Arrange
    repo = DownloadRepository(db)
    manager = DownloadManager(settings, repo)
    try:
        record = manager.add_download(http_server.url("/files/data.bin"), "")

        # Act
        dispatched = manager.start_all()

        # Assert (poll until terminal)
        assert dispatched == 1
        settled: DownloadRecord | None = None
        deadline = time.monotonic() + 30.0
        while time.monotonic() < deadline:
            settled = repo.get(record.id or -1)
            assert settled is not None
            if settled.state in TERMINAL_STATES:
                break
            time.sleep(0.05)
        assert settled is not None
        assert settled.state is DownloadState.COMPLETED
        final = pathlib.Path(settled.destination_path)
        assert final.read_bytes() == http_server.file_bytes("data.bin")
    finally:
        manager.shutdown(wait=True)

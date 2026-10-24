"""Tests for ``src.core.download_manager``: intake validation, concurrency, retries, lifecycle."""

from __future__ import annotations

import pathlib
import time

import pytest

from src.core.download_manager import DownloadManager
from src.database.models import TERMINAL_STATES, DownloadState, Settings
from src.database.repository import DownloadRepository
from src.database.connection import Database


@pytest.fixture()
def manager(settings: Settings, db: Database) -> DownloadManager:
    """A real DownloadManager wired to the temp database; shut down on teardown."""
    repo = DownloadRepository(db)
    mgr = DownloadManager(settings, repo)
    yield mgr
    mgr.shutdown(wait=True)


def _wait_terminal(repo: DownloadRepository, ids: list[int], timeout: float = 30.0) -> None:
    """Block until every id reaches a terminal state (or fail the test)."""
    remaining = dict.fromkeys(ids)
    deadline = time.monotonic() + timeout
    while remaining and time.monotonic() < deadline:
        for download_id in list(remaining):
            record = repo.get(download_id)
            assert record is not None, f"record {download_id} vanished"
            if record.state in TERMINAL_STATES:
                del remaining[download_id]
        if remaining:
            time.sleep(0.05)
    assert not remaining, f"downloads never settled: {sorted(remaining)}"


# -- intake validation --------------------------------------------------------------


@pytest.mark.parametrize(
    "bad_url", ["ftp://host.example/file.zip", "not a url", ""]
)
def test_add_download_rejects_unusable_urls(
    manager: DownloadManager, bad_url: str
) -> None:
    with pytest.raises(ValueError):
        manager.add_download(bad_url, "whatever")


# -- bounded concurrency ----------------------------------------------------------


def test_concurrent_completions_respect_parallel_limit(
    manager: DownloadManager, settings: Settings, db: Database, tmp_path: pathlib.Path, http_server
) -> None:
    # Arrange: five downloads, distinct destination candidates.
    repo = DownloadRepository(db)
    ids = [
        manager.add_download(
            http_server.url("/files/data.bin"), str(tmp_path / "dl"), filename=f"f{i}.bin"
        ).id
        for i in range(5)
    ]
    assert all(isinstance(i, int) for i in ids)
    manager.on_progress = None  # default; no observer needed

    # Act
    dispatched = manager.start_all()

    # Assert
    assert dispatched == manager.max_simultaneous == settings.max_simultaneous == 3
    _wait_terminal(repo, list(ids))
    for download_id in ids:
        record = repo.get(download_id)
        assert record is not None
        assert record.state is DownloadState.COMPLETED
        final = pathlib.Path(record.destination_path)
        assert final.read_bytes() == http_server.file_bytes("data.bin")
        assert not pathlib.Path(str(final) + ".part").exists()
    assert 1 <= http_server.max_parallel <= 3
    assert manager.list_active() == []


# -- transient failures + retry ------------------------------------------------------


def test_status_500_exhausts_retries_then_fails_again_on_retry(
    manager: DownloadManager, db: Database, tmp_path: pathlib.Path, http_server
) -> None:
    # Arrange
    repo = DownloadRepository(db)
    record = manager.add_download(
        http_server.url("/status/500"), str(tmp_path / "dl"), filename="broken.bin"
    )
    assert record.id is not None

    # Act + Assert: exactly retry_count attempts (3), then permanent failure.
    manager.start_all()
    _wait_terminal(repo, [record.id])
    failed = repo.get(record.id)
    assert failed is not None
    assert failed.state is DownloadState.FAILED
    assert failed.retry_count == 3
    assert failed.error_summary is not None
    assert "500" in failed.error_summary

    # Act again: manager.retry resets the bookkeeping and fails cleanly again.
    manager.retry(record.id)
    _wait_terminal(repo, [record.id])
    failed2 = repo.get(record.id)
    assert failed2 is not None
    assert failed2.state is DownloadState.FAILED
    assert failed2.retry_count == 3
    assert failed2.error_summary is not None
    assert "500" in failed2.error_summary


# -- cancel / remove lifecycle --------------------------------------------------------


def test_cancel_queued_download(manager: DownloadManager, db: Database, tmp_path: pathlib.Path, http_server) -> None:
    # Arrange
    repo = DownloadRepository(db)
    record = manager.add_download(
        http_server.url("/files/data.bin"), str(tmp_path / "dl"), filename="queued.bin"
    )
    assert record.id is not None
    assert record.state is DownloadState.QUEUED

    # Act
    manager.cancel(record.id)

    # Assert
    cancelled = repo.get(record.id)
    assert cancelled is not None
    assert cancelled.state is DownloadState.CANCELLED


def test_remove_semantics(
    manager: DownloadManager, settings: Settings, db: Database, tmp_path: pathlib.Path, http_server
) -> None:
    # Arrange: settled rows are removable; running ones are not.
    repo = DownloadRepository(db)
    record = manager.add_download(
        http_server.url("/files/data.bin"), str(tmp_path / "dl"), filename="removeme.bin"
    )
    assert record.id is not None

    # Act + Assert: queued row removes once, then re-removing is False.
    assert manager.remove(record.id) is True
    assert manager.remove(record.id) is False
    assert repo.get(record.id) is None

    # A running download refuses removal until cancellation settles it.
    record2 = manager.add_download(
        http_server.url("/slow"), str(tmp_path / "dl"), filename="running.bin"
    )
    assert record2.id is not None
    manager.start_all()
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        current = repo.get(record2.id)
        assert current is not None
        if current.state is DownloadState.DOWNLOADING:
            break
        time.sleep(0.005)
    else:
        pytest.fail("download never entered DOWNLOADING state")
    with pytest.raises(ValueError):
        manager.remove(record2.id)
    manager.cancel(record2.id)
    _wait_terminal(repo, [record2.id])
    cancelled = repo.get(record2.id)
    assert cancelled is not None
    assert cancelled.state is DownloadState.CANCELLED
    assert manager.remove(record2.id) is True


# -- misc manager surface ----------------------------------------------------------


def test_total_speed_nonnegative_and_no_active_after_settle(
    manager: DownloadManager, db: Database, tmp_path: pathlib.Path, http_server
) -> None:
    # Arrange + Act
    repo = DownloadRepository(db)
    record = manager.add_download(
        http_server.url("/files/small.zip"), str(tmp_path / "dl"), filename="tiny.zip"
    )
    assert record.id is not None
    manager.start_all()
    _wait_terminal(repo, [record.id])
    settled = repo.get(record.id)
    assert settled is not None
    assert settled.state is DownloadState.COMPLETED

    # Assert
    assert manager.total_speed_bps() >= 0
    assert manager.list_active() == []

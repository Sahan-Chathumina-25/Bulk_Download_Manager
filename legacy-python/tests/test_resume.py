"""Tests for engine resume behaviour: Range/206 append, 200 restart, ETag If-Range, 416 finalize."""

from __future__ import annotations

import pathlib

import pytest

from src.core.download_worker import DownloadWorker
from src.core import filename_utils
from src.database.models import DownloadRecord, DownloadState, Settings


@pytest.fixture()
def dl_dir(tmp_path: pathlib.Path) -> pathlib.Path:
    """Where resume-test destination files live."""
    return tmp_path / "dl"


def _make_record(url: str, dest_dir: pathlib.Path, filename: str) -> DownloadRecord:
    """A worker-ready record with the destination path pre-sanitised."""
    name = filename_utils.sanitize_filename(filename)
    destination = filename_utils.resolve_destination(str(dest_dir), name)
    return DownloadRecord(
        source_url=url,
        display_name=name,
        destination_path=str(destination),
        state=DownloadState.DOWNLOADING,
    )


def test_manual_part_resumes_with_single_range_request(
    http_server, settings: Settings, dl_dir: pathlib.Path
) -> None:
    # Arrange: a 700-byte manual .part plus matching bookkeeping.
    data = http_server.file_bytes("data.bin")
    record = _make_record(
        http_server.url("/files/data.bin"), dl_dir, "data.bin"
    )
    part = pathlib.Path(record.destination_path + ".part")
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(data[:700])
    record.downloaded_bytes = 700
    http_server.clear_hits()
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert: one request (the 206 resume), full file published, .part gone.
    assert settled.state is DownloadState.COMPLETED
    assert http_server.hits("/files/data.bin") == 1
    final = pathlib.Path(settled.destination_path)
    assert final.read_bytes() == data
    assert not part.exists()


def test_server_ignoring_range_restarts_fresh(
    http_server, settings: Settings, dl_dir: pathlib.Path
) -> None:
    # Arrange: partial .part against the /norange endpoint (200 + no ranges).
    data = http_server.file_bytes("data.bin")
    record = _make_record(
        http_server.url("/norange/data.bin"), dl_dir, "data.bin"
    )
    part = pathlib.Path(record.destination_path + ".part")
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(data[:700])
    record.downloaded_bytes = 700
    http_server.clear_hits()
    worker = DownloadWorker(record, settings)

    # Act
    settled = worker.run()

    # Assert: the stale part can't be appended, so the file restarts from zero.
    assert settled.state is DownloadState.COMPLETED
    assert http_server.hits("/norange/data.bin") == 2
    final = pathlib.Path(settled.destination_path)
    assert final.read_bytes() == data
    assert not part.exists()


def test_changed_etag_forces_restart_and_captures_new_validator(
    http_server, settings: Settings, dl_dir: pathlib.Path
) -> None:
    # Arrange: .part built against an outdated ETag ("e0"); server now at "e1".
    http_server.set_file("swap.bin", b"A" * 2048)
    http_server.set_etag("swap.bin", '"e1"')
    record = _make_record(
        http_server.url("/etag/swap.bin"), dl_dir, "swap.bin"
    )
    part = pathlib.Path(record.destination_path + ".part")
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(b"A" * 700)
    record.downloaded_bytes = 700
    record.etag = '"e0"'
    http_server.clear_hits()
    worker = DownloadWorker(record, settings)

    # Act: stale If-Range -> full 200 -> restart from zero.
    settled = worker.run()

    # Assert
    assert settled.state is DownloadState.COMPLETED
    assert http_server.hits("/etag/swap.bin") == 2
    final = pathlib.Path(settled.destination_path)
    assert final.read_bytes() == b"A" * 2048
    assert settled.etag == '"e1"'
    assert not part.exists()


def test_complete_part_finalizes_via_416(http_server, settings: Settings, dl_dir: pathlib.Path) -> None:
    # Arrange: the .part already holds the entire remote file.
    data = http_server.file_bytes("data.bin")
    record = _make_record(
        http_server.url("/files/data.bin"), dl_dir, "data.bin"
    )
    part = pathlib.Path(record.destination_path + ".part")
    part.parent.mkdir(parents=True, exist_ok=True)
    part.write_bytes(data)
    record.downloaded_bytes = len(data)
    http_server.clear_hits()
    worker = DownloadWorker(record, settings)

    # Act: bytes=<total>- is unsatisfiable -> 416 + authoritative size.
    settled = worker.run()

    # Assert: finalize without transferring any bytes.
    assert settled.state is DownloadState.COMPLETED
    assert http_server.hits("/files/data.bin") == 1
    final = pathlib.Path(settled.destination_path)
    assert final.read_bytes() == data
    assert not part.exists()

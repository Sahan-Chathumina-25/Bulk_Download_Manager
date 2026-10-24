"""Tests for ``src.database.repository``: CRUD, settings persistence,
startup recovery, history queries and history clearing."""

from __future__ import annotations

import pathlib

import pytest

from src.database.connection import Database
from src.database.models import DownloadRecord, DownloadState, Settings
from src.database.repository import (
    HISTORY_STATES,
    SORTABLE_COLUMNS,
    DownloadRepository,
)


@pytest.fixture()
def repo(db: Database) -> DownloadRepository:
    """A repository facade over the schema-initialised temp database."""
    return DownloadRepository(db)


def _full_record(tmp_path: pathlib.Path) -> DownloadRecord:
    """A DownloadRecord with every field deliberately populated."""
    return DownloadRecord(
        source_url="http://example.invalid/files/report.zip",
        display_name="My Report (2026).zip",
        destination_path=str(tmp_path / "report.zip"),
        state=DownloadState.FAILED,
        downloaded_bytes=482,
        expected_size=16384,
        created_at="2026-01-02T03:04:05+00:00",
        completed_at=None,
        retry_count=2,
        error_summary="HTTP 500 after 3 attempts",
        etag='"report-16384"',
        last_modified="Mon, 02 Jan 2026 03:00:00 GMT",
        checksum_sha256="a" * 64,
        checksum_verified=True,
        content_type="application/zip",
    )


# -- DownloadRecord roundtrip -------------------------------------------------------


def test_full_field_download_record_roundtrip(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange
    record = _full_record(tmp_path)

    # Act
    stored = repo.add(record)
    fetched = repo.get(stored.id or 0)

    # Assert: every single field survives the database roundtrip.
    assert fetched is not None
    for name in (
        "id", "source_url", "display_name", "destination_path", "state",
        "downloaded_bytes", "expected_size", "created_at", "completed_at",
        "retry_count", "error_summary", "etag", "last_modified",
        "checksum_sha256", "checksum_verified", "content_type",
    ):
        assert getattr(fetched, name) == getattr(record, name), f"field {name!r} drifted"
    assert fetched == record  # id/created_at were supplied, so full equality holds


def test_add_blank_row_gets_id_and_created_at(repo: DownloadRepository) -> None:
    # Arrange
    record = DownloadRecord(source_url="http://example.invalid/a.zip")

    # Act
    stored = repo.add(record)
    fetched = repo.get(stored.id or 0)

    # Assert: id assigned and created_at backfilled with a sortable ISO stamp.
    assert fetched is not None
    assert fetched.id == stored.id
    assert fetched.created_at
    assert fetched.state is DownloadState.QUEUED


# -- Settings save/load roundtrip ----------------------------------------------------


def test_settings_roundtrip_and_none_before_first_save(
    repo: DownloadRepository, settings: Settings
) -> None:
    # Assert: no settings row yet -> get_settings is None.
    assert repo.get_settings() is None

    # Arrange: a fully modified settings object.
    saved = Settings(
        default_destination_dir=str(settings.default_destination_dir),
        max_simultaneous=7,
        connection_timeout=11.5,
        retry_count=5,
        retry_delay=0.25,
        buffer_size=32768,
        bandwidth_limit_bps=1048576,
        filename_collision="rename",
        notifications=False,
        theme="light",
        user_agent="TestAgent/1.0",
        include_unknown_types=True,
        allowed_extensions={"zip", "pdf"},
        respect_rate_limit=False,
        allow_private_addresses=True,
    )

    # Act
    repo.save_settings(saved)
    loaded = repo.get_settings()

    # Assert: roundtrip equality implies the row was loaded (non-None).
    assert loaded == saved


# -- recover_interrupted ---------------------------------------------------------


def test_recover_interrupted_clamps_to_part_size(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange: a row that died mid-download with stale bookkeeping.
    dest = tmp_path / "big.zip"
    record = repo.add(
        DownloadRecord(
            source_url="http://example.invalid/files/big.zip",
            display_name="big.zip",
            destination_path=str(dest),
            state=DownloadState.DOWNLOADING,
            downloaded_bytes=123,  # stale: disk has more
            expected_size=16384,
        )
    )
    pathlib.Path(str(dest) + ".part").write_bytes(b"x" * 500)

    # Act
    recovered = repo.recover_interrupted()

    # Assert
    assert [r.id for r in recovered] == [record.id]
    assert recovered[0].state is DownloadState.INTERRUPTED
    assert recovered[0].downloaded_bytes == 500
    fetched = repo.get(record.id or 0)
    assert fetched is not None
    assert fetched.state is DownloadState.INTERRUPTED
    assert fetched.downloaded_bytes == 500

    # Idempotency: a second run reconciles nothing.
    assert repo.recover_interrupted() == []


def test_recover_interrupted_no_files_means_zero_bytes(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange: row in DOWNLOADING, but nothing was ever written to disk.
    record = repo.add(
        DownloadRecord(
            source_url="http://example.invalid/files/gone.zip",
            display_name="gone.zip",
            destination_path=str(tmp_path / "gone.zip"),
            state=DownloadState.DOWNLOADING,
            downloaded_bytes=123,
            expected_size=1024,
        )
    )

    # Act
    recovered = repo.recover_interrupted()

    # Assert: interrupted at zero bytes — never completed from nothing.
    assert [r.id for r in recovered] == [record.id]
    assert recovered[0].state is not DownloadState.COMPLETED
    assert recovered[0].downloaded_bytes == 0
    fetched = repo.get(record.id or 0)
    assert fetched is not None
    assert fetched.state is DownloadState.INTERRUPTED
    assert fetched.downloaded_bytes == 0
    assert fetched.completed_at is None


def test_recover_interrupted_complete_file_on_disk_marks_completed(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange: the final file already exists with exactly the expected size.
    dest = tmp_path / "done.bin"
    dest.write_bytes(b"z" * 2048)
    record = repo.add(
        DownloadRecord(
            source_url="http://example.invalid/files/data.bin",
            display_name="done.bin",
            destination_path=str(dest),
            state=DownloadState.DOWNLOADING,
            downloaded_bytes=100,  # stale; disk is authoritative
            expected_size=2048,
        )
    )

    # Act
    recovered = repo.recover_interrupted()

    # Assert: disk is the source of truth -> completed with completion time.
    assert [r.id for r in recovered] == [record.id]
    assert recovered[0].state is DownloadState.COMPLETED
    assert recovered[0].completed_at is not None
    assert recovered[0].downloaded_bytes == 2048
    fetched = repo.get(record.id or 0)
    assert fetched is not None
    assert fetched.state is DownloadState.COMPLETED

    # Idempotency: running again recovers nothing.
    assert repo.recover_interrupted() == []


def test_recover_interrupted_skips_empty_database_and_settled_rows(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange: only an already-terminal row.
    done = repo.add(
        DownloadRecord(
            source_url="http://example.invalid/f.zip",
            display_name="f.zip",
            destination_path=str(tmp_path / "f.zip"),
            state=DownloadState.COMPLETED,
            downloaded_bytes=10,
            expected_size=10,
            completed_at="2026-01-01T00:00:00+00:00",
        )
    )

    # Act
    recovered = repo.recover_interrupted()

    # Assert: COMPLETED rows are never touched.
    assert recovered == []
    fetched = repo.get(done.id or 0)
    assert fetched is not None
    assert fetched.state is DownloadState.COMPLETED
    assert fetched.downloaded_bytes == 10


# -- history() --------------------------------------------------------------------


def _seed_history(repo: DownloadRepository) -> dict[str, int]:
    """Insert settled + active rows; return display_name -> created id."""
    seeds: list[DownloadRecord] = [
        DownloadRecord(
            source_url="http://example.invalid/a.zip",
            display_name="alpha.zip",
            state=DownloadState.COMPLETED,
            downloaded_bytes=300,
            expected_size=300,
        ),
        DownloadRecord(
            source_url="http://example.invalid/b.zip",
            display_name="bravo zip report",
            state=DownloadState.FAILED,
            downloaded_bytes=100,
            error_summary="HTTP 500",
        ),
        DownloadRecord(
            source_url="http://example.invalid/c.zip",
            display_name="charlie.zip",
            state=DownloadState.CANCELLED,
            downloaded_bytes=200,
        ),
        DownloadRecord(
            source_url="http://example.invalid/d.zip",
            display_name="delta.zip",
            state=DownloadState.INTERRUPTED,
            downloaded_bytes=400,
        ),
        DownloadRecord(
            source_url="http://example.invalid/e.zip",
            display_name="echo.zip",
            state=DownloadState.QUEUED,
            downloaded_bytes=999,  # active: must be invisible to history()
        ),
        DownloadRecord(
            source_url="http://example.invalid/g.zip",
            display_name="foxtrot.zip",
            state=DownloadState.DOWNLOADING,
            downloaded_bytes=1,  # active: must be invisible to history()
        ),
    ]
    return {rec.display_name: (repo.add(rec).id or 0) for rec in seeds}


def test_history_excludes_active_rows(repo: DownloadRepository) -> None:
    # Arrange: settled rows (alpha/bravo/charlie/delta) + active rows (echo/foxtrot).
    _seed_history(repo)

    # Act
    rows = repo.history(limit=0)

    # Assert: TERMINAL + INTERRUPTED are in, QUEUED/DOWNLOADING are out.
    assert HISTORY_STATES == {DownloadState.COMPLETED, DownloadState.FAILED,
                              DownloadState.CANCELLED, DownloadState.INTERRUPTED}
    names = {r.display_name for r in rows}
    assert names == {"alpha.zip", "bravo zip report", "charlie.zip", "delta.zip"}
    assert SORTABLE_COLUMNS >= {"downloaded_bytes"}
    assert all(r.state not in (DownloadState.QUEUED, DownloadState.DOWNLOADING)
               for r in rows)


def test_history_filter_by_state(repo: DownloadRepository) -> None:
    _seed_history(repo)

    # Act + Assert: enum filter lands only the FAILED row.
    failed = repo.history(state=DownloadState.FAILED, limit=0)
    assert [r.display_name for r in failed] == ["bravo zip report"]

    # String state values work too.
    completed = repo.history(state="completed", limit=0)
    assert [r.display_name for r in completed] == ["alpha.zip"]


def test_history_search_substring(repo: DownloadRepository) -> None:
    ids = _seed_history(repo)

    # Act + Assert: case-insensitive display_name substring.
    rows = repo.history(search="REPORT")
    assert [r.display_name for r in rows] == ["bravo zip report"]
    assert [r.id for r in rows] == [ids["bravo zip report"]]


def test_history_sort_by_downloaded_bytes_desc(repo: DownloadRepository) -> None:
    _seed_history(repo)

    # Act + Assert: visible rows sorted 400, 300, 200, 100.
    rows = repo.history(sort_by="downloaded_bytes", sort_dir="DESC", limit=0)
    assert [r.downloaded_bytes for r in rows] == [400, 300, 200, 100]

    ascending = repo.history(sort_by="downloaded_bytes", sort_dir="ASC", limit=0)
    assert [r.downloaded_bytes for r in ascending] == [100, 200, 300, 400]


def test_history_sort_validation(
    repo: DownloadRepository,
) -> None:
    _seed_history(repo)

    for kwargs in ({"sort_by": "downloaded_bytes; DROP TABLE users"},
                   {"sort_by": "not-a-column"},
                   {"sort_dir": "sideways"}):
        with pytest.raises(ValueError):
            repo.history(**kwargs)  # type: ignore[arg-type]


# -- clear_history -----------------------------------------------------------------


def _seed_completed_with_file(tmp_path: pathlib.Path, repo: DownloadRepository,
                              filename: str) -> pathlib.Path:
    """A COMPLETED row pointing at a real on-disk file; returns the file path."""
    dest = tmp_path / filename
    dest.write_bytes(b"payload")
    repo.add(
        DownloadRecord(
            source_url=f"http://example.invalid/files/{filename}",
            display_name=filename,
            destination_path=str(dest),
            state=DownloadState.COMPLETED,
            downloaded_bytes=7,
            expected_size=7,
            completed_at="2026-01-01T00:00:00+00:00",
        )
    )
    return dest


def test_clear_history_keep_files_preserves_disk(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange
    dest = _seed_completed_with_file(tmp_path, repo, "keepme.zip")

    # Act
    removed = repo.clear_history(keep_files=True)

    # Assert: rows gone, artifact untouched on disk.
    assert removed == 1
    assert repo.history(limit=0) == []
    assert dest.is_file()


def test_clear_history_without_keep_files_deletes_only_destination_files(
    repo: DownloadRepository, tmp_path: pathlib.Path
) -> None:
    # Arrange: two history rows + an unrelated sibling in the same directory.
    keep = _seed_completed_with_file(tmp_path, repo, "first.zip")
    doomed = _seed_completed_with_file(tmp_path, repo, "second.zip")
    sibling = tmp_path / "sibling.txt"
    sibling.write_text("not a download")

    # Act
    removed = repo.clear_history(keep_files=False)

    # Assert: both rows and their destination files are gone...
    assert removed == 2
    assert repo.history(limit=0) == []
    assert not keep.exists()
    assert not doomed.exists()
    # ...but the unrelated file next to them survives.
    assert sibling.is_file()
    assert sibling.read_text() == "not a download"

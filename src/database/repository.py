"""Parameterized SQL repository for downloads and settings.

All queries are bound-parameter queries — string values are never
interpolated into SQL. Sorting columns are validated against a whitelist
before they touch a query. Disk reconciliation (``recover_interrupted``)
never marks a download COMPLETED from the database alone; the final file
must exist on disk with the expected size.
"""

from __future__ import annotations

import logging
import pathlib
import json
from datetime import UTC, datetime

from src.database.connection import Database
from src.database.models import (
    TERMINAL_STATES,
    DownloadRecord,
    DownloadState,
    Settings,
)

__all__ = ["HISTORY_STATES", "SORTABLE_COLUMNS", "DownloadRepository"]

log = logging.getLogger(__name__)

#: States shown in the "history" view (finished + interrupted-at-restart).
HISTORY_STATES: frozenset[DownloadState] = frozenset(
    {*TERMINAL_STATES, DownloadState.INTERRUPTED}
)

#: Columns history queries may sort by (SQL identifier whitelist).
SORTABLE_COLUMNS: frozenset[str] = frozenset(
    {
        "id", "created_at", "completed_at", "state", "display_name",
        "destination_path", "downloaded_bytes", "expected_size", "retry_count",
    }
)


def _now_iso() -> str:
    """Current UTC time as an ISO-8601 string (sortable)."""
    return datetime.now(tz=UTC).isoformat(timespec="seconds")


def _record_from_row(row) -> DownloadRecord:  # noqa: ANN001 - sqlite3.Row
    """Build a :class:`DownloadRecord` from a ``downloads`` row."""
    return DownloadRecord(
        id=int(row["id"]),
        source_url=row["source_url"] or "",
        display_name=row["display_name"] or "",
        destination_path=row["destination_path"] or "",
        state=DownloadState(row["state"]),
        downloaded_bytes=int(row["downloaded_bytes"] or 0),
        expected_size=int(row["expected_size"]) if row["expected_size"] is not None else None,
        created_at=row["created_at"] or "",
        completed_at=row["completed_at"],
        retry_count=int(row["retry_count"] or 0),
        error_summary=row["error_summary"],
        etag=row["etag"],
        last_modified=row["last_modified"],
        checksum_sha256=row["checksum_sha256"],
        checksum_verified=bool(row["checksum_verified"]),
        content_type=row["content_type"],
    )


_RECORD_FIELDS = (
    "source_url", "display_name", "destination_path", "state",
    "downloaded_bytes", "expected_size", "created_at", "completed_at",
    "retry_count", "error_summary", "etag", "last_modified",
    "checksum_sha256", "checksum_verified", "content_type",
)

_INSERT_SQL = (
    "INSERT INTO downloads ({fields}) "
    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
).format(fields=", ".join(_RECORD_FIELDS))

_UPDATE_SQL = (
    "UPDATE downloads SET {assignments} WHERE id = ?"
).format(
    assignments=", ".join(f"{name} = ?" for name in _RECORD_FIELDS)
)


def _params_from_record(rec: DownloadRecord) -> tuple:
    """Serialize a record into the column order used by INSERT/UPDATE."""
    return (
        rec.source_url,
        rec.display_name,
        rec.destination_path,
        rec.state.value,
        int(rec.downloaded_bytes),
        int(rec.expected_size) if rec.expected_size is not None else None,
        rec.created_at,
        rec.completed_at,
        int(rec.retry_count),
        rec.error_summary,
        rec.etag,
        rec.last_modified,
        rec.checksum_sha256,
        1 if rec.checksum_verified else 0,
        rec.content_type,
    )


class DownloadRepository:
    """CRUD + query facade over the ``downloads`` and ``settings`` tables."""

    def __init__(self, db: Database) -> None:
        self._db = db

    # -- downloads CRUD --------------------------------------------------
    def add(self, record: DownloadRecord) -> DownloadRecord:
        """INSERT ``record``; assigns ``id`` and stamps ``created_at`` when blank."""
        if not record.created_at:
            record.created_at = _now_iso()  # persisted with the row insert
        record.id = self._db.execute(_INSERT_SQL, _params_from_record(record))
        return record

    def get(self, download_id: int) -> DownloadRecord | None:
        """Fetch one record by id or ``None``."""
        row = self._db.query_one("SELECT * FROM downloads WHERE id = ?", (download_id,))
        return _record_from_row(row) if row is not None else None

    def find_by_url(self, url: str) -> DownloadRecord | None:
        """First record matching an exact source URL or ``None``."""
        row = self._db.query_one(
            "SELECT * FROM downloads WHERE source_url = ? ORDER BY id LIMIT 1", (url,)
        )
        return _record_from_row(row) if row is not None else None

    def list_all(self) -> list[DownloadRecord]:
        """Every download, oldest first."""
        return [
            _record_from_row(row)
            for row in self._db.query("SELECT * FROM downloads ORDER BY id")
        ]

    def list_by_state(self, *states: DownloadState) -> list[DownloadRecord]:
        """All downloads whose state is any of ``states`` (oldest first)."""
        if not states:
            return []
        placeholders = ", ".join("?" for _ in states)
        rows = self._db.query(
            f"SELECT * FROM downloads WHERE state IN ({placeholders}) ORDER BY id",
            tuple(s.value for s in states),
        )
        return [_record_from_row(row) for row in rows]

    def update(self, record: DownloadRecord) -> bool:
        """Persist all fields of ``record``; ``False`` if the row is gone."""
        with self._db.transaction() as conn:
            cur = conn.execute(_UPDATE_SQL, (*_params_from_record(record), record.id))
            changed = cur.rowcount > 0
        return changed

    def update_state(self, download_id: int, state: DownloadState) -> None:
        """Change only the ``state`` column (cheap transition persistence)."""
        self._db.execute(
            "UPDATE downloads SET state = ? WHERE id = ?", (state.value, download_id)
        )

    def delete(self, download_id: int) -> bool:
        """Delete one download row; ``False`` if it did not exist."""
        with self._db.transaction() as conn:
            cur = conn.execute("DELETE FROM downloads WHERE id = ?", (download_id,))
            return cur.rowcount > 0

    # -- history ---------------------------------------------------------
    def history(
        self,
        *,
        state: str | DownloadState | None = None,
        search: str | None = None,
        sort_by: str = "created_at",
        sort_dir: str = "DESC",
        limit: int = 100,
        offset: int = 0,
    ) -> list[DownloadRecord]:
        """History rows with filtering, search, sorting and pagination.

        Args:
            state: optionally restrict to one state.
            search: case-insensitive match against display name, file name
                or source URL (LIKE-escaped: ``%``/``_`` are literal).
            sort_by: one of :data:`SORTABLE_COLUMNS`.
            sort_dir: ``ASC`` or ``DESC``.
            limit/offset: page window (``limit=0`` means no limit).

        Raises:
            ValueError: on unknown sort column or direction.
        """
        sort_by = str(sort_by).strip()
        if sort_by not in SORTABLE_COLUMNS:
            raise ValueError(
                f"sort_by must be one of {sorted(SORTABLE_COLUMNS)}, got {sort_by!r}"
            )
        sort_dir = str(sort_dir).upper()
        if sort_dir not in ("ASC", "DESC"):
            raise ValueError(f"sort_dir must be ASC or DESC, got {sort_dir!r}")
        int(limit), int(offset)  # fail fast on junk before touching SQL

        where: list[str] = []
        params: list[object] = []
        if state is not None:
            state_value = state.value if isinstance(state, DownloadState) else str(state)
            where.append("state = ?")
            params.append(state_value)
        else:
            placeholders = ", ".join("?" for _ in sorted(HISTORY_STATES))
            where.append(f"state IN ({placeholders})")
            params.extend(s.value for s in sorted(HISTORY_STATES))
        if search:
            escaped = str(search).replace("\\", "\\\\").replace("%", r"\%").replace("_", r"\_")
            pattern = f"%{escaped}%"
            where.append(
                "(display_name LIKE ? ESCAPE '\\' "
                "OR destination_path LIKE ? ESCAPE '\\' "
                "OR source_url LIKE ? ESCAPE '\\')"
            )
            params.extend([pattern, pattern, pattern])

        limit_value = int(limit)
        sql = (
            f"SELECT * FROM downloads WHERE {' AND '.join(where)} "
            f"ORDER BY {sort_by} COLLATE NOCASE {sort_dir}, id {sort_dir} "
        )
        if limit_value > 0:
            sql += "LIMIT ? OFFSET ?"
            params.extend([limit_value, int(offset)])
        rows = self._db.query(sql, tuple(params))
        return [_record_from_row(row) for row in rows]

    def clear_history(self, *, keep_files: bool = True) -> int:
        """Delete history rows; count of deleted rows is returned.

        Args:
            keep_files: when ``False``, the destination file (the user-visible
                download artifact) of every deleted row is also removed from
                disk. Only destination files are touched — never anything else
                in the directory. Files that no longer exist are ignored.
        """
        rows = self.history()
        removed = 0
        for record in rows:
            if not keep_files and record.destination_path:
                with OSErrorGuard():
                    pathlib.Path(record.destination_path).unlink(missing_ok=True)
            if self.delete(record.id or 0):
                removed += 1
        return removed

    # -- startup recovery ------------------------------------------------
    def recover_interrupted(self) -> list[DownloadRecord]:
        """Reconcile states after an app restart; returns recovered rows.

        - Rows stuck in ``CONNECTING``/``DOWNLOADING`` become ``INTERRUPTED``.
        - When the final file already exists on disk with the expected size,
          the row becomes ``COMPLETED`` (disk is the source of truth — never
          the database alone).
        - ``downloaded_bytes`` is clamped to the actual size of the ``.part``
          file so a later resume never trusts stale bookkeeping.

        Idempotent: running it again on an already-recovered database is a
        no-op for fully reconciled rows.
        """
        recovered_ids: list[DownloadState] = []
        touched: list[DownloadRecord] = []
        stale = self.list_by_state(
            DownloadState.CONNECTING, DownloadState.DOWNLOADING, DownloadState.INTERRUPTED
        )
        now = _now_iso()
        for record in stale:
            before_state = record.state
            if before_state in (DownloadState.CONNECTING, DownloadState.DOWNLOADING):
                record.state = DownloadState.INTERRUPTED
                recovered_ids.append(before_state)
            if record.state is DownloadState.INTERRUPTED:
                changed = self._reconcile_files(record, now)
                self.update(record)
                if record.state is not before_state or changed:
                    touched.append(record)
        for _state in recovered_ids:
            log.info("app restarted while a download was %s", _state.value)
        return touched

    def _reconcile_files(self, record: DownloadRecord, now_iso: str) -> bool:
        """Adjust one INTERRUPTED record against files on disk (True=changed)."""
        final = pathlib.Path(record.destination_path) if record.destination_path else None
        part = pathlib.Path(f"{record.destination_path}.part") if final else None
        part_size = part.stat().st_size if (part and part.is_file()) else None
        final_size = final.stat().st_size if (final and final.is_file()) else None
        changed = False

        if final_size is not None and (
            record.expected_size is None
            or record.expected_size <= 0
            or final_size == record.expected_size
        ):
            record.state = DownloadState.COMPLETED
            record.completed_at = record.completed_at or now_iso
            record.downloaded_bytes = final_size
            log.info("download %s: found complete file on disk, marking completed", record.id)
            return True
        if part_size is not None:
            changed_before = record.downloaded_bytes
            record.downloaded_bytes = part_size
            log.info(
                "download %s: %d bytes of partial data on disk, keeping INTERRUPTED",
                record.id, part_size,
            )
            return changed_before != part_size
        if record.downloaded_bytes != 0:
            record.downloaded_bytes = 0
            changed = True
        return changed

    # -- settings --------------------------------------------------------
    def get_settings(self) -> Settings | None:
        """Load the single settings row (validated) or ``None`` when unset."""
        row = self._db.query_one("SELECT * FROM settings WHERE id = 1")
        if row is None:
            return None
        try:
            return Settings(
                default_destination_dir=row["default_destination_dir"] or "",
                max_simultaneous=int(row["max_simultaneous"]),
                connection_timeout=float(row["connection_timeout"]),
                retry_count=int(row["retry_count"]),
                retry_delay=float(row["retry_delay"]),
                buffer_size=int(row["buffer_size"]),
                bandwidth_limit_bps=int(row["bandwidth_limit_bps"]),
                filename_collision=row["filename_collision"],
                notifications=bool(row["notifications"]),
                theme=row["theme"],
                user_agent=row["user_agent"],
                include_unknown_types=bool(row["include_unknown_types"]),
                allowed_extensions=set(json.loads(row["allowed_extensions"] or "[]")),
                respect_rate_limit=bool(row["respect_rate_limit"]),
                allow_private_addresses=bool(row["allow_private_addresses"]),
            )
        except (ValueError, TypeError, KeyError) as exc:
            log.error("stored settings are invalid (%s); falling back to defaults", exc)
            return None

    def save_settings(self, settings: Settings) -> None:
        """Upsert the single settings row from a validated ``Settings``."""
        payload = (
            settings.default_destination_dir,
            int(settings.max_simultaneous),
            float(settings.connection_timeout),
            int(settings.retry_count),
            float(settings.retry_delay),
            int(settings.buffer_size),
            int(settings.bandwidth_limit_bps),
            settings.filename_collision,
            1 if settings.notifications else 0,
            settings.theme,
            settings.user_agent,
            1 if settings.include_unknown_types else 0,
            json.dumps(sorted(settings.allowed_extensions)),
            1 if settings.respect_rate_limit else 0,
            1 if settings.allow_private_addresses else 0,
        )
        self._db.execute(
            """
            INSERT INTO settings (
                id, default_destination_dir, max_simultaneous, connection_timeout,
                retry_count, retry_delay, buffer_size, bandwidth_limit_bps,
                filename_collision, notifications, theme, user_agent,
                include_unknown_types, allowed_extensions, respect_rate_limit,
                allow_private_addresses
            ) VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                default_destination_dir=excluded.default_destination_dir,
                max_simultaneous=excluded.max_simultaneous,
                connection_timeout=excluded.connection_timeout,
                retry_count=excluded.retry_count,
                retry_delay=excluded.retry_delay,
                buffer_size=excluded.buffer_size,
                bandwidth_limit_bps=excluded.bandwidth_limit_bps,
                filename_collision=excluded.filename_collision,
                notifications=excluded.notifications,
                theme=excluded.theme,
                user_agent=excluded.user_agent,
                include_unknown_types=excluded.include_unknown_types,
                allowed_extensions=excluded.allowed_extensions,
                respect_rate_limit=excluded.respect_rate_limit,
                allow_private_addresses=excluded.allow_private_addresses
            """,
            payload,
        )

    def close(self) -> None:
        """Close the underlying database connection."""
        self._db.close()


class OSErrorGuard:
    """Context manager that logs (never raises) expected filesystem errors."""

    def __enter__(self) -> "OSErrorGuard":
        return self

    def __exit__(self, exc_type, exc, _tb) -> bool:  # noqa: ANN001
        if isinstance(exc, (OSError, ValueError)):
            log.warning("filesystem operation skipped: %s", exc)
            return True
        return False

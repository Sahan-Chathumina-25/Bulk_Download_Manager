"""Thread-safe SQLite connection wrapper (WAL, foreign keys, busy timeout).

Every method takesAND releases a re-entrant lock, so a single connection can
be shared between worker threads and the GUI. Column definitions mirror the
dataclasses in :mod:`src.database.models` exactly.
"""

from __future__ import annotations

import logging
import pathlib
import sqlite3
import threading
from contextlib import contextmanager
from collections.abc import Iterator

__all__ = ["Database", "SCHEMA_VERSION", "init_db"]

log = logging.getLogger(__name__)

SCHEMA_VERSION = 1

_DOWNLOADS_SCHEMA = """
CREATE TABLE IF NOT EXISTS downloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_url TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    destination_path TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'queued',
    downloaded_bytes INTEGER NOT NULL DEFAULT 0,
    expected_size INTEGER,
    created_at TEXT NOT NULL DEFAULT '',
    completed_at TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    error_summary TEXT,
    etag TEXT,
    last_modified TEXT,
    checksum_sha256 TEXT,
    checksum_verified INTEGER NOT NULL DEFAULT 0,
    content_type TEXT
)
"""

_SETTINGS_SCHEMA = """
CREATE TABLE IF NOT EXISTS settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    default_destination_dir TEXT NOT NULL DEFAULT '',
    max_simultaneous INTEGER NOT NULL DEFAULT 3,
    connection_timeout REAL NOT NULL DEFAULT 30.0,
    retry_count INTEGER NOT NULL DEFAULT 3,
    retry_delay REAL NOT NULL DEFAULT 2.0,
    buffer_size INTEGER NOT NULL DEFAULT 65536,
    bandwidth_limit_bps INTEGER NOT NULL DEFAULT 0,
    filename_collision TEXT NOT NULL DEFAULT 'keep_existing',
    notifications INTEGER NOT NULL DEFAULT 1,
    theme TEXT NOT NULL DEFAULT 'dark',
    user_agent TEXT NOT NULL DEFAULT '',
    include_unknown_types INTEGER NOT NULL DEFAULT 0,
    allowed_extensions TEXT NOT NULL DEFAULT '[]',
    respect_rate_limit INTEGER NOT NULL DEFAULT 1,
    allow_private_addresses INTEGER NOT NULL DEFAULT 0
)
"""

_INDEX_SCHEMA = """
CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state);
CREATE INDEX IF NOT EXISTS idx_downloads_created_at ON downloads(created_at);
"""


class Database:
    """A lock-protected wrapper around one SQLite connection."""

    def __init__(self, path: str | pathlib.Path) -> None:
        """Open (or create) the database at ``path`` and apply PRAGMAs.

        ``:memory:`` and ``file:`` style URIs are passed through to sqlite3
        untouched; real paths are created (parent directories included).
        """
        self._raw = path if isinstance(path, str) else str(path)
        self._is_virtual = self._raw in (":memory:", "") or self._raw.startswith("file:")
        if not self._is_virtual:
            pathlib.Path(self._raw).parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.RLock()
        self._conn = sqlite3.connect(self._raw, check_same_thread=False, timeout=30.0)
        self._conn.row_factory = sqlite3.Row
        self._conn.isolation_level = None  # explicit transactions only
        with self._lock:
            self._conn.execute("PRAGMA journal_mode=WAL")
            self._conn.execute("PRAGMA foreign_keys=ON")
            self._conn.execute("PRAGMA busy_timeout=5000")
            self._conn.execute("PRAGMA synchronous=NORMAL")

    # -- basic info ------------------------------------------------------
    @property
    def path(self) -> str:
        """Raw path (or URI / ``:memory:``) this database was opened from."""
        return self._raw

    # -- primitives ------------------------------------------------------
    def execute(self, sql: str, params: tuple | dict = ()) -> int:
        """Run a single statement, commit, and return ``cursor.lastrowid``."""
        with self._lock:
            cur = self._conn.execute(sql, params)
            self._conn.commit()
            return int(cur.lastrowid or 0)

    def query(self, sql: str, params: tuple | dict = ()) -> list[sqlite3.Row]:
        """Run a statement and return all result rows (inside the lock)."""
        with self._lock:
            return self._conn.execute(sql, params).fetchall()

    def query_one(self, sql: str, params: tuple | dict = ()) -> sqlite3.Row | None:
        """Run a statement and return the first result row or ``None``."""
        with self._lock:
            return self._conn.execute(sql, params).fetchone()

    @contextmanager
    def transaction(self) -> Iterator[sqlite3.Connection]:
        """Explicit transaction: lock held, ``BEGIN IMMEDIATE`` .. commit/rollback."""
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                yield self._conn
            except Exception:
                self._conn.execute("ROLLBACK")
                raise
            self._conn.execute("COMMIT")

    # -- DDL -------------------------------------------------------------
    def ensure_schema(self) -> None:
        """Create all tables/indexes if absent (idempotent)."""
        with self._lock:
            self._conn.executescript(_DOWNLOADS_SCHEMA)
            self._conn.executescript(_SETTINGS_SCHEMA)
            self._conn.executescript(_INDEX_SCHEMA)
            self._conn.commit()

    def close(self) -> None:
        """Flush and close the connection (safe to call more than once)."""
        with self._lock:
            try:
                self._conn.close()
            except sqlite3.Error as exc:  # pragma: no cover - close errors are rare
                log.warning("error closing sqlite database: %s", exc)


def init_db(path: str | pathlib.Path) -> Database:
    """Open the database at ``path`` and create the Phase-1 schema."""
    db = Database(path)
    db.ensure_schema()
    log.info("database ready at %s", path)
    return db


if __name__ == "__main__":  # pragma: no cover - manual smoke test
    init_db(pathlib.Path("bulk-download-manager.db"))

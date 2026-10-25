//! Lock-protected SQLite connection wrapper (WAL, foreign keys, busy timeout).
//!
//! Port of the Python ``database.connection`` module. Python shares one
//! ``sqlite3.Connection`` guarded by an ``RLock``; here the same single
//! connection lives behind a [`Mutex`], which is the Rust equivalent.
//!
//! There is no explicit ``close()``: dropping the [`Database`] drops the
//! [`Connection`], which closes it (a Rust-idiomatic divergence from the
//! Python wrapper).

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, ToSql};

use crate::database::repository::RepositoryError;

/// Schema revision written by this build.
pub const SCHEMA_VERSION: i64 = 1;

const DOWNLOADS_SCHEMA: &str = "
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
";

const SETTINGS_SCHEMA: &str = "
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
";

const INDEX_SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state);
CREATE INDEX IF NOT EXISTS idx_downloads_created_at ON downloads(created_at);
";

/// A lock-protected wrapper around one SQLite connection.
///
/// The single [`Connection`] is shared behind a [`Mutex`] because the Python
/// original used an ``RLock`` for the same purpose: worker threads and the
/// GUI both call through the same connection. All access is synchronous, so
/// the guard is never held across an ``await``.
pub struct Database {
    path: String,
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) the database at ``path`` and apply PRAGMAs.
    ///
    /// ``:memory:`` and the empty string open an in-memory database without
    /// touching the filesystem. Every other value is treated as a literal
    /// path whose parent directories are created first (Python's ``file:``
    /// URI handling is intentionally not reproduced).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let path = path.as_ref();
        let raw = path.to_string_lossy().into_owned();
        let is_virtual = raw.is_empty() || raw == ":memory:";
        if !is_virtual
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let conn = if is_virtual {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        conn.busy_timeout(Duration::from_millis(5_000))?;
        // WAL degrades to "memory" on an in-memory database without error.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\nPRAGMA foreign_keys=ON;\nPRAGMA synchronous=NORMAL;",
        )?;
        Ok(Self {
            path: raw,
            conn: Mutex::new(conn),
        })
    }

    /// Open a private in-memory database (convenience for ``:memory:``).
    pub fn open_in_memory() -> Result<Self, RepositoryError> {
        Self::open(":memory:")
    }

    /// Open the database at ``path`` and create the schema (port of ``init_db``).
    pub fn init_db(path: impl AsRef<Path>) -> Result<Database, RepositoryError> {
        let db = Self::open(path)?;
        db.ensure_schema()?;
        tracing::info!(path = %db.path, "database ready");
        Ok(db)
    }

    /// Raw path (or ``:memory:``) this database was opened from.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Create all tables/indexes if absent (idempotent).
    pub fn ensure_schema(&self) -> Result<(), RepositoryError> {
        let conn = self.lock();
        conn.execute_batch(DOWNLOADS_SCHEMA)?;
        conn.execute_batch(SETTINGS_SCHEMA)?;
        conn.execute_batch(INDEX_SCHEMA)?;
        Ok(())
    }

    /// Run a single statement and return ``last_insert_rowid()``.
    ///
    /// Mirrors Python's ``int(cur.lastrowid or 0)``: rusqlite returns 0 when
    /// no row was inserted.
    pub fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<i64, RepositoryError> {
        let conn = self.lock();
        conn.execute(sql, params)?;
        Ok(conn.last_insert_rowid())
    }

    /// Run ``body`` while holding the connection lock (used for queries).
    pub fn with_conn<T>(
        &self,
        body: impl FnOnce(&Connection) -> Result<T, RepositoryError>,
    ) -> Result<T, RepositoryError> {
        let conn = self.lock();
        body(&conn)
    }

    /// Explicit transaction: lock held, ``BEGIN IMMEDIATE`` .. commit/rollback.
    ///
    /// Mirrors the Python context manager: commit on ``Ok``, roll back on
    /// ``Err`` (rollback failures are logged and ignored) and propagate.
    pub fn transaction<T>(
        &self,
        body: impl FnOnce(&Connection) -> Result<T, RepositoryError>,
    ) -> Result<T, RepositoryError> {
        let conn = self.lock();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match body(&conn) {
            Ok(value) => {
                conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(err) => {
                if let Err(rollback) = conn.execute_batch("ROLLBACK") {
                    tracing::warn!(error = %rollback, "rollback failed");
                }
                Err(err)
            }
        }
    }

    /// A panic while holding the lock is a bug, so poisoning is unrecoverable.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("database mutex poisoned")
    }
}

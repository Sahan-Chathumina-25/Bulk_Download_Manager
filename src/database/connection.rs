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
///
/// v2 added `downloads.failure_category` plus the two ad-filter settings
/// columns; see [`ensure_column`] for the in-place migration.
///
/// v3 added the `file_analyses` table used by the AI File Finder to persist
/// the last analysis runs (bounded to a handful of runs, pruned on write).
pub const SCHEMA_VERSION: i64 = 3;

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
    content_type TEXT,
    failure_category TEXT
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
    allow_private_addresses INTEGER NOT NULL DEFAULT 0,
    ad_filter_enabled INTEGER NOT NULL DEFAULT 0,
    ad_filter_list_path TEXT NOT NULL DEFAULT ''
)
";

const INDEX_SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state);
CREATE INDEX IF NOT EXISTS idx_downloads_created_at ON downloads(created_at);
";

const FILE_ANALYSES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS file_analyses (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    analysis_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    archive_type TEXT NOT NULL DEFAULT '',
    score INTEGER NOT NULL DEFAULT 0,
    classification TEXT NOT NULL DEFAULT '',
    group_id TEXT,
    part_number INTEGER,
    evidence_json TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT ''
)
";

const FILE_ANALYSES_INDEX_SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS idx_file_analyses_created_at ON file_analyses(created_at);
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
        // Never rewrite a database written by a newer build: `ensure_schema`
        // would stamp `user_version` down and the older code could corrupt
        // rows whose shape it does not understand.
        let user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if user_version > SCHEMA_VERSION {
            return Err(RepositoryError::SchemaTooNew {
                found: user_version,
                supported: SCHEMA_VERSION,
            });
        }
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

    /// Create all tables/indexes if absent and migrate older databases.
    ///
    /// `CREATE TABLE IF NOT EXISTS` never alters an existing table, so new
    /// columns need an explicit, idempotent `ALTER TABLE` (see
    /// [`ensure_column`]).
    pub fn ensure_schema(&self) -> Result<(), RepositoryError> {
        let conn = self.lock();
        conn.execute_batch(DOWNLOADS_SCHEMA)?;
        conn.execute_batch(SETTINGS_SCHEMA)?;
        conn.execute_batch(INDEX_SCHEMA)?;
        ensure_column(&conn, "downloads", "failure_category", "TEXT")?;
        ensure_column(
            &conn,
            "settings",
            "ad_filter_enabled",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &conn,
            "settings",
            "ad_filter_list_path",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        conn.execute_batch(FILE_ANALYSES_SCHEMA)?;
        conn.execute_batch(FILE_ANALYSES_INDEX_SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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

/// Add `column` to `table` when the database predates it.
///
/// `PRAGMA table_info` lists the existing columns, so the migration runs at
/// most once per database and is a no-op on freshly created files (every
/// migrated column is already part of the `CREATE TABLE` statements). Rows
/// written before the upgrade keep the column's `NULL`/default value.
///
/// `table` and `column` are compile-time literals supplied by this module (and
/// are covered by the tests below); they are never user input.
fn ensure_column(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
    declaration: &str,
) -> Result<(), RepositoryError> {
    let mut present = false;
    {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name.eq_ignore_ascii_case(column) {
                present = true;
                break;
            }
        }
    }
    if !present {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
        tracing::info!(table, column, "schema migration added column");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "bdm-connection-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("downloads.db")
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        // Arrange: create a database and stamp it with a future schema.
        let path = temp_db("newer");
        let database = Database::open(&path).expect("open database");
        database
            .with_conn(|conn| {
                conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)?;
                Ok(())
            })
            .expect("bump schema version");
        drop(database);

        // Act
        let error = match Database::open(&path) {
            Ok(_) => panic!("a newer schema must be refused"),
            Err(error) => error,
        };

        // Assert
        assert!(
            error.to_string().contains("newer version"),
            "error was {error:?}"
        );
    }

    #[test]
    fn current_schema_version_reopens_cleanly() {
        let path = temp_db("current");
        Database::init_db(&path).expect("initialise");
        Database::open(&path).expect("reopen at the current schema");
    }

    #[test]
    fn file_analyses_table_and_index_are_created() {
        let path = temp_db("analysis-table");
        let database = Database::init_db(&path).expect("initialise");
        let (table, index): (i64, i64) = database
            .with_conn(|conn| {
                let table: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'file_analyses'",
                    [],
                    |row| row.get(0),
                )?;
                let index: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_file_analyses_created_at'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((table, index))
            })
            .expect("query sqlite master");
        assert_eq!(table, 1, "file_analyses table missing");
        assert_eq!(index, 1, "file_analyses index missing");
    }
}

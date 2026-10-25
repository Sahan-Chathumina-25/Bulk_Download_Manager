//! Parameterized SQL repository for downloads and settings.
//!
//! Port of the Python ``database.repository`` module. All queries bind their
//! values as parameters; string values are never interpolated into SQL. The
//! only interpolated SQL fragments are the whitelisted ``sort_by`` and
//! ``sort_dir`` identifiers (validated against [`SORTABLE_COLUMNS`] and
//! ``ASC``/``DESC`` respectively, so they cannot carry injected SQL).

use std::path::Path;
use std::sync::Arc;

use rusqlite::types::Value;
use rusqlite::{Row, Rows, ToSql, params, params_from_iter};
use thiserror::Error;

use crate::config::{CollisionPolicy, Settings, Theme};
use crate::database::connection::Database;
use crate::models::download_item::{DownloadRecord, now_iso};
use crate::models::download_status::DownloadState;

/// Errors raised by the repository layer.
#[derive(Debug, Error)]
pub enum RepositoryError {
    /// Underlying SQLite failure.
    #[error(transparent)]
    Db(#[from] rusqlite::Error),

    /// Filesystem failure (directory creation, file inspection, ...).
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    /// A history query used a column outside the whitelist.
    #[error("sort_by must be one of {columns:?}, got {got:?}")]
    InvalidSortColumn {
        columns: Vec<&'static str>,
        got: String,
    },

    /// A history query used a direction other than ``ASC``/``DESC``.
    #[error("sort_dir must be ASC or DESC, got {got:?}")]
    InvalidSortDirection { got: String },

    /// A downloads row stores a state string this build does not know.
    #[error("corrupt downloads row {id}: {message}")]
    CorruptRow { id: i64, message: String },
}

/// Columns history queries may sort by (SQL identifier whitelist).
///
/// Kept in alphabetical order because the Python error message prints
/// ``sorted(SORTABLE_COLUMNS)``.
pub const SORTABLE_COLUMNS: [&str; 9] = [
    "completed_at",
    "created_at",
    "destination_path",
    "display_name",
    "downloaded_bytes",
    "expected_size",
    "id",
    "retry_count",
    "state",
];

/// States shown in the "history" view (finished + interrupted-at-restart).
///
/// Ordered alphabetically by stored value, matching the Python
/// ``sorted(HISTORY_STATES)`` placeholder order.
pub const HISTORY_STATES: [DownloadState; 4] = [
    DownloadState::Cancelled,
    DownloadState::Completed,
    DownloadState::Failed,
    DownloadState::Interrupted,
];

const INSERT_SQL: &str = "INSERT INTO downloads (source_url, display_name, destination_path, state, downloaded_bytes, expected_size, created_at, completed_at, retry_count, error_summary, etag, last_modified, checksum_sha256, checksum_verified, content_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const UPDATE_SQL: &str = "UPDATE downloads SET source_url = ?, display_name = ?, destination_path = ?, state = ?, downloaded_bytes = ?, expected_size = ?, created_at = ?, completed_at = ?, retry_count = ?, error_summary = ?, etag = ?, last_modified = ?, checksum_sha256 = ?, checksum_verified = ?, content_type = ? WHERE id = ?";

const SETTINGS_UPSERT_SQL: &str = "
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
";

/// Owned values plus borrows that make up the 15 column bindings of a record.
struct RecordParams<'a> {
    record: &'a DownloadRecord,
    state: &'static str,
    downloaded_bytes: i64,
    expected_size: Option<i64>,
    retry_count: i64,
    checksum_verified: i64,
}

impl<'a> RecordParams<'a> {
    fn new(record: &'a DownloadRecord) -> Self {
        Self {
            record,
            state: record.state.as_str(),
            downloaded_bytes: to_i64(record.downloaded_bytes),
            expected_size: record.expected_size.map(to_i64),
            retry_count: i64::from(record.retry_count),
            checksum_verified: bool_to_i64(record.checksum_verified),
        }
    }

    /// Bindings in the exact column order used by INSERT/UPDATE.
    fn values(&self) -> [&dyn ToSql; 15] {
        [
            &self.record.source_url,
            &self.record.display_name,
            &self.record.destination_path,
            &self.state,
            &self.downloaded_bytes,
            &self.expected_size,
            &self.record.created_at,
            &self.record.completed_at,
            &self.retry_count,
            &self.record.error_summary,
            &self.record.etag,
            &self.record.last_modified,
            &self.record.checksum_sha256,
            &self.checksum_verified,
            &self.record.content_type,
        ]
    }
}

/// Filters, ordering and paging for [`DownloadRepository::history`].
pub struct HistoryQuery {
    pub state: Option<String>,
    pub search: Option<String>,
    pub sort_by: String,
    pub sort_dir: String,
    pub limit: usize,
    pub offset: usize,
}

// Written manually: deriving would make `limit` default to 0 (unbounded),
// but the Python default is 100 rows.
impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            state: None,
            search: None,
            sort_by: "created_at".to_string(),
            sort_dir: "DESC".to_string(),
            limit: 100,
            offset: 0,
        }
    }
}

/// CRUD + query facade over the ``downloads`` and ``settings`` tables.
pub struct DownloadRepository {
    db: Arc<Database>,
}

impl DownloadRepository {
    /// Wrap a shared database handle (the Python repository holds a strong
    /// reference to its ``Database`` as well).
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // -- downloads CRUD --------------------------------------------------

    /// INSERT ``record``; assigns ``id`` and stamps ``created_at`` when blank.
    pub fn add(&self, record: &mut DownloadRecord) -> Result<(), RepositoryError> {
        if record.created_at.is_empty() {
            record.created_at = now_iso();
        }
        let params = RecordParams::new(record);
        let values = params.values();
        let id = self.db.execute(INSERT_SQL, &values)?;
        record.id = Some(id);
        Ok(())
    }

    /// Fetch one record by id or ``None``.
    pub fn get(&self, download_id: i64) -> Result<Option<DownloadRecord>, RepositoryError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM downloads WHERE id = ?")?;
            let mut rows = stmt.query([download_id])?;
            match rows.next()? {
                Some(row) => Ok(Some(record_from_row(row)?)),
                None => Ok(None),
            }
        })
    }

    /// First record matching an exact source URL or ``None``.
    ///
    /// Duplicate URLs are allowed (there is no unique constraint); the
    /// lowest id wins, matching the Python ``ORDER BY id LIMIT 1``.
    pub fn find_by_url(&self, url: &str) -> Result<Option<DownloadRecord>, RepositoryError> {
        self.db.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT * FROM downloads WHERE source_url = ? ORDER BY id LIMIT 1")?;
            let mut rows = stmt.query([url])?;
            match rows.next()? {
                Some(row) => Ok(Some(record_from_row(row)?)),
                None => Ok(None),
            }
        })
    }

    /// Every download, oldest first.
    pub fn list_all(&self) -> Result<Vec<DownloadRecord>, RepositoryError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM downloads ORDER BY id")?;
            let mut rows = stmt.query(params![])?;
            collect_records(&mut rows)
        })
    }

    /// All downloads whose state is any of ``states`` (oldest first).
    pub fn list_by_state(
        &self,
        states: &[DownloadState],
    ) -> Result<Vec<DownloadRecord>, RepositoryError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; states.len()].join(", ");
        let sql = format!("SELECT * FROM downloads WHERE state IN ({placeholders}) ORDER BY id");
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut rows =
                stmt.query(params_from_iter(states.iter().map(|state| state.as_str())))?;
            collect_records(&mut rows)
        })
    }

    /// Persist all fields of ``record``; ``false`` if the row is gone.
    pub fn update(&self, record: &DownloadRecord) -> Result<bool, RepositoryError> {
        let params = RecordParams::new(record);
        let mut values = params.values().to_vec();
        let id = record.id;
        values.push(&id);
        self.db.transaction(|conn| {
            let changed = conn.execute(UPDATE_SQL, values.as_slice())?;
            Ok(changed > 0)
        })
    }

    /// Change only the ``state`` column (cheap transition persistence).
    pub fn update_state(
        &self,
        download_id: i64,
        state: DownloadState,
    ) -> Result<(), RepositoryError> {
        let state_value = state.as_str();
        let bindings: [&dyn ToSql; 2] = [&state_value, &download_id];
        self.db
            .execute("UPDATE downloads SET state = ? WHERE id = ?", &bindings)?;
        Ok(())
    }

    /// Delete one download row; ``false`` if it did not exist.
    pub fn delete(&self, download_id: i64) -> Result<bool, RepositoryError> {
        self.db.transaction(|conn| {
            let changed = conn.execute("DELETE FROM downloads WHERE id = ?", [download_id])?;
            Ok(changed > 0)
        })
    }

    // -- history ---------------------------------------------------------

    /// History rows with filtering, search, sorting and pagination.
    ///
    /// ``limit == 0`` means "no limit"; otherwise ``LIMIT ? OFFSET ?`` is
    /// appended.
    pub fn history(&self, query: HistoryQuery) -> Result<Vec<DownloadRecord>, RepositoryError> {
        let sort_by = query.sort_by.trim().to_string();
        if !SORTABLE_COLUMNS.contains(&sort_by.as_str()) {
            return Err(RepositoryError::InvalidSortColumn {
                columns: SORTABLE_COLUMNS.to_vec(),
                got: sort_by,
            });
        }
        let sort_dir = query.sort_dir.to_uppercase();
        if sort_dir != "ASC" && sort_dir != "DESC" {
            return Err(RepositoryError::InvalidSortDirection { got: sort_dir });
        }

        let mut where_parts: Vec<&str> = Vec::new();
        let mut bindings: Vec<Value> = Vec::new();

        match query.state {
            Some(state) => {
                where_parts.push("state = ?");
                bindings.push(Value::Text(state));
            }
            None => {
                where_parts.push("state IN (?, ?, ?, ?)");
                for state in HISTORY_STATES {
                    bindings.push(Value::Text(state.as_str().to_string()));
                }
            }
        }

        if let Some(search) = query.search.as_deref().filter(|value| !value.is_empty()) {
            let escaped = search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("%{escaped}%");
            where_parts.push(
                "(display_name LIKE ? ESCAPE '\\' OR destination_path LIKE ? ESCAPE '\\' OR source_url LIKE ? ESCAPE '\\')",
            );
            bindings.push(Value::Text(pattern.clone()));
            bindings.push(Value::Text(pattern.clone()));
            bindings.push(Value::Text(pattern));
        }

        // `sort_by` and `sort_dir` are whitelisted identifiers, so interpolating
        // them is safe; every value above is still bound as a parameter.
        let mut sql = format!(
            "SELECT * FROM downloads WHERE {} ORDER BY {} COLLATE NOCASE {}, id {}",
            where_parts.join(" AND "),
            sort_by,
            sort_dir,
            sort_dir,
        );
        if query.limit > 0 {
            sql.push_str(" LIMIT ? OFFSET ?");
            bindings.push(Value::Integer(
                i64::try_from(query.limit).unwrap_or(i64::MAX),
            ));
            bindings.push(Value::Integer(
                i64::try_from(query.offset).unwrap_or(i64::MAX),
            ));
        }

        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(params_from_iter(bindings))?;
            collect_records(&mut rows)
        })
    }

    /// Delete history rows; returns the number of deleted rows.
    ///
    /// When ``keep_files`` is ``false``, each deleted row's destination file
    /// is also removed; files that no longer exist are ignored and other
    /// files in the directory are never touched.
    ///
    /// Legacy quirk preserved: this uses the default history window
    /// (``limit = 100``), so a single call deletes at most 100 rows.
    pub fn clear_history(&self, keep_files: bool) -> Result<usize, RepositoryError> {
        let rows = self.history(HistoryQuery::default())?;
        let mut removed = 0;
        for record in rows {
            if !keep_files && !record.destination_path.is_empty() {
                match std::fs::remove_file(&record.destination_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        tracing::warn!(
                            path = %record.destination_path,
                            error = %err,
                            "could not delete download file"
                        );
                    }
                }
            }
            if self.delete(record.id.unwrap_or(0))? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    // -- startup recovery ------------------------------------------------

    /// Reconcile states after an app restart; returns recovered rows.
    ///
    /// Rows stuck in ``CONNECTING``/``DOWNLOADING`` become ``INTERRUPTED``;
    /// a final file already on disk with the expected size marks the row
    /// ``COMPLETED`` (disk is the source of truth); ``downloaded_bytes`` is
    /// clamped to the actual ``.part`` size. Idempotent by construction.
    pub fn recover_interrupted(&self) -> Result<Vec<DownloadRecord>, RepositoryError> {
        let stale = self.list_by_state(&[
            DownloadState::Connecting,
            DownloadState::Downloading,
            DownloadState::Interrupted,
        ])?;
        let now = now_iso();
        let mut touched = Vec::new();
        for mut record in stale {
            let before_state = record.state;
            if matches!(
                before_state,
                DownloadState::Connecting | DownloadState::Downloading
            ) {
                record.state = DownloadState::Interrupted;
            }
            if record.state == DownloadState::Interrupted {
                let changed = reconcile_files(&mut record, &now);
                self.update(&record)?;
                if record.state != before_state || changed {
                    touched.push(record);
                }
            }
        }
        Ok(touched)
    }

    // -- settings --------------------------------------------------------

    /// Load the single settings row (validated) or ``None`` when unset.
    ///
    /// A corrupt row (bad JSON, unknown enum, failed validation) is logged
    /// and treated as "unset"; it is never propagated as an error, matching
    /// the Python ``except (ValueError, TypeError, KeyError)`` fallback.
    pub fn get_settings(&self) -> Result<Option<Settings>, RepositoryError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT * FROM settings WHERE id = 1")?;
            let mut rows = stmt.query(params![])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            match settings_from_row(row) {
                Ok(settings) => Ok(Some(settings)),
                Err(message) => {
                    tracing::error!(
                        error = %message,
                        "stored settings are invalid; falling back to defaults"
                    );
                    Ok(None)
                }
            }
        })
    }

    /// Upsert the single settings row from a validated ``Settings``.
    pub fn save_settings(&self, settings: &Settings) -> Result<(), RepositoryError> {
        let max_simultaneous = i64::from(settings.max_simultaneous);
        let connection_timeout = settings.connection_timeout;
        let retry_count = i64::from(settings.retry_count);
        let retry_delay = settings.retry_delay;
        let buffer_size = i64::from(settings.buffer_size);
        let bandwidth_limit_bps = i64::try_from(settings.bandwidth_limit_bps).unwrap_or(i64::MAX);
        let collision = settings.filename_collision.as_str();
        let notifications = bool_to_i64(settings.notifications);
        let theme = settings.theme.as_str();
        let include_unknown_types = bool_to_i64(settings.include_unknown_types);
        // serde_json omits the spaces Python's json.dumps adds after commas;
        // both documents parse back identically.
        let allowed_extensions = {
            let sorted: Vec<String> = settings.allowed_extensions.iter().cloned().collect();
            serde_json::to_string(&sorted).expect("allowed_extensions serialise infallibly")
        };
        let respect_rate_limit = bool_to_i64(settings.respect_rate_limit);
        let allow_private_addresses = bool_to_i64(settings.allow_private_addresses);

        let bindings: [&dyn ToSql; 15] = [
            &settings.default_destination_dir,
            &max_simultaneous,
            &connection_timeout,
            &retry_count,
            &retry_delay,
            &buffer_size,
            &bandwidth_limit_bps,
            &collision,
            &notifications,
            &theme,
            &settings.user_agent,
            &include_unknown_types,
            &allowed_extensions,
            &respect_rate_limit,
            &allow_private_addresses,
        ];
        self.db.execute(SETTINGS_UPSERT_SQL, &bindings)?;
        Ok(())
    }
}

/// Build a [`DownloadRecord`] from a ``downloads`` row (port of ``_record_from_row``).
///
/// Nullable text columns default to ``""``; nullable counters default to 0;
/// signed values are clamped into the model's unsigned types (the Python
/// dataclass accepted negatives, but the Rust model cannot represent them).
fn record_from_row(row: &Row<'_>) -> Result<DownloadRecord, RepositoryError> {
    let id: i64 = row.get("id")?;
    let state_text: String = row.get("state")?;
    let state = DownloadState::parse(&state_text).ok_or_else(|| RepositoryError::CorruptRow {
        id,
        message: format!("unknown download state {state_text:?}"),
    })?;

    let source_url: Option<String> = row.get("source_url")?;
    let display_name: Option<String> = row.get("display_name")?;
    let destination_path: Option<String> = row.get("destination_path")?;
    let downloaded_bytes: Option<i64> = row.get("downloaded_bytes")?;
    let expected_size: Option<i64> = row.get("expected_size")?;
    let created_at: Option<String> = row.get("created_at")?;
    let completed_at: Option<String> = row.get("completed_at")?;
    let retry_count: Option<i64> = row.get("retry_count")?;
    let error_summary: Option<String> = row.get("error_summary")?;
    let etag: Option<String> = row.get("etag")?;
    let last_modified: Option<String> = row.get("last_modified")?;
    let checksum_sha256: Option<String> = row.get("checksum_sha256")?;
    let checksum_verified: Option<i64> = row.get("checksum_verified")?;
    let content_type: Option<String> = row.get("content_type")?;

    Ok(DownloadRecord {
        id: Some(id),
        source_url: source_url.unwrap_or_default(),
        display_name: display_name.unwrap_or_default(),
        destination_path: destination_path.unwrap_or_default(),
        state,
        downloaded_bytes: non_negative_u64(downloaded_bytes.unwrap_or(0)),
        expected_size: expected_size.map(non_negative_u64),
        created_at: created_at.unwrap_or_default(),
        completed_at,
        retry_count: non_negative_u32(retry_count.unwrap_or(0)),
        error_summary,
        etag,
        last_modified,
        checksum_sha256,
        checksum_verified: checksum_verified.map(|value| value != 0).unwrap_or(false),
        content_type,
    })
}

/// Adjust one INTERRUPTED record against files on disk (``true`` = changed).
fn reconcile_files(record: &mut DownloadRecord, now_iso: &str) -> bool {
    let final_path = if record.destination_path.is_empty() {
        None
    } else {
        Some(Path::new(&record.destination_path))
    };
    let part_path = final_path.map(|_| {
        let mut raw = record.destination_path.clone();
        raw.push_str(".part");
        std::path::PathBuf::from(raw)
    });

    let part_size = part_path.as_deref().and_then(file_size);
    let final_size = final_path.and_then(file_size);

    if let Some(size) = final_size {
        let expected_matches = match record.expected_size {
            None | Some(0) => true,
            Some(expected) => size == expected,
        };
        if expected_matches {
            record.state = DownloadState::Completed;
            if record.completed_at.is_none() {
                record.completed_at = Some(now_iso.to_string());
            }
            record.downloaded_bytes = size;
            return true;
        }
    }
    if let Some(size) = part_size {
        let before = record.downloaded_bytes;
        record.downloaded_bytes = size;
        return before != size;
    }
    if record.downloaded_bytes != 0 {
        record.downloaded_bytes = 0;
        return true;
    }
    false
}

/// Size of a regular file, or ``None`` when it is absent/unreadable.
fn file_size(path: &Path) -> Option<u64> {
    if path.is_file() {
        std::fs::metadata(path).ok().map(|metadata| metadata.len())
    } else {
        None
    }
}

/// Rebuild settings from a stored row; any failure is a descriptive string.
fn settings_from_row(row: &Row<'_>) -> Result<Settings, String> {
    let default_destination_dir: Option<String> = row
        .get("default_destination_dir")
        .map_err(|err| err.to_string())?;
    let max_simultaneous: i64 = row.get("max_simultaneous").map_err(|err| err.to_string())?;
    let connection_timeout: f64 = row
        .get("connection_timeout")
        .map_err(|err| err.to_string())?;
    let retry_count: i64 = row.get("retry_count").map_err(|err| err.to_string())?;
    let retry_delay: f64 = row.get("retry_delay").map_err(|err| err.to_string())?;
    let buffer_size: i64 = row.get("buffer_size").map_err(|err| err.to_string())?;
    let bandwidth_limit_bps: i64 = row
        .get("bandwidth_limit_bps")
        .map_err(|err| err.to_string())?;
    let filename_collision: String = row
        .get("filename_collision")
        .map_err(|err| err.to_string())?;
    let notifications: i64 = row.get("notifications").map_err(|err| err.to_string())?;
    let theme_text: String = row.get("theme").map_err(|err| err.to_string())?;
    let user_agent: String = row.get("user_agent").map_err(|err| err.to_string())?;
    let include_unknown_types: i64 = row
        .get("include_unknown_types")
        .map_err(|err| err.to_string())?;
    let allowed_extensions: Option<String> = row
        .get("allowed_extensions")
        .map_err(|err| err.to_string())?;
    let respect_rate_limit: i64 = row
        .get("respect_rate_limit")
        .map_err(|err| err.to_string())?;
    let allow_private_addresses: i64 = row
        .get("allow_private_addresses")
        .map_err(|err| err.to_string())?;

    let collision = CollisionPolicy::parse(&filename_collision)
        .ok_or_else(|| format!("unknown filename_collision {filename_collision:?}"))?;
    let theme = Theme::parse(&theme_text).ok_or_else(|| format!("unknown theme {theme_text:?}"))?;
    let extensions: Vec<String> =
        serde_json::from_str(allowed_extensions.as_deref().unwrap_or("[]"))
            .map_err(|err| format!("invalid allowed_extensions JSON: {err}"))?;

    Settings {
        default_destination_dir: default_destination_dir.unwrap_or_default(),
        max_simultaneous: u32::try_from(max_simultaneous.clamp(1, 10))
            .map_err(|err| err.to_string())?,
        connection_timeout,
        retry_count: u32::try_from(retry_count)
            .map_err(|_| format!("retry_count out of range: {retry_count}"))?,
        retry_delay,
        buffer_size: u32::try_from(buffer_size)
            .map_err(|_| format!("buffer_size out of range: {buffer_size}"))?,
        bandwidth_limit_bps: u64::try_from(bandwidth_limit_bps)
            .map_err(|_| format!("bandwidth_limit_bps out of range: {bandwidth_limit_bps}"))?,
        filename_collision: collision,
        notifications: notifications != 0,
        theme,
        user_agent,
        include_unknown_types: include_unknown_types != 0,
        allowed_extensions: extensions.into_iter().collect(),
        respect_rate_limit: respect_rate_limit != 0,
        allow_private_addresses: allow_private_addresses != 0,
    }
    .validated()
}

/// Drain a row cursor into records.
fn collect_records(rows: &mut Rows<'_>) -> Result<Vec<DownloadRecord>, RepositoryError> {
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        records.push(record_from_row(row)?);
    }
    Ok(records)
}

/// Bind a u64 as a signed SQLite integer (sizes fit comfortably in i64).
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Map a negative SQLite integer to 0 (the Rust model is unsigned).
fn non_negative_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Map a negative SQLite integer to 0 and clamp absurdly large values.
fn non_negative_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;
    use crate::config::{CollisionPolicy, Settings, Theme};
    use crate::database::connection::Database;
    use crate::models::download_item::DownloadRecord;
    use crate::models::download_status::DownloadState;

    /// A schema-initialised database in its own temp directory (the conftest
    /// ``db`` fixture equivalent).
    fn test_repo(name: &str) -> (PathBuf, Arc<Database>, DownloadRepository) {
        let dir = std::env::temp_dir().join(format!("bdm-db-tests-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let db = Arc::new(Database::init_db(dir.join("downloads.db")).expect("init db"));
        let repo = DownloadRepository::new(Arc::clone(&db));
        (dir, db, repo)
    }

    /// A COMPLETED row pointing at a real on-disk file; returns the file path.
    fn seed_completed_with_file(dir: &Path, repo: &DownloadRepository, filename: &str) -> PathBuf {
        let destination = dir.join(filename);
        std::fs::write(&destination, b"payload").expect("write payload");
        let mut record = DownloadRecord {
            source_url: format!("http://example.invalid/files/{filename}"),
            display_name: filename.to_string(),
            destination_path: destination.to_string_lossy().into_owned(),
            state: DownloadState::Completed,
            downloaded_bytes: 7,
            expected_size: Some(7),
            completed_at: Some("2026-01-01T00:00:00+00:00".to_string()),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add completed");
        destination
    }

    /// Insert settled + active rows; return display_name -> created id.
    fn seed_history(repo: &DownloadRepository) -> HashMap<String, i64> {
        let seeds = [
            DownloadRecord {
                source_url: "http://example.invalid/a.zip".to_string(),
                display_name: "alpha.zip".to_string(),
                state: DownloadState::Completed,
                downloaded_bytes: 300,
                expected_size: Some(300),
                ..DownloadRecord::default()
            },
            DownloadRecord {
                source_url: "http://example.invalid/b.zip".to_string(),
                display_name: "bravo zip report".to_string(),
                state: DownloadState::Failed,
                downloaded_bytes: 100,
                error_summary: Some("HTTP 500".to_string()),
                ..DownloadRecord::default()
            },
            DownloadRecord {
                source_url: "http://example.invalid/c.zip".to_string(),
                display_name: "charlie.zip".to_string(),
                state: DownloadState::Cancelled,
                downloaded_bytes: 200,
                ..DownloadRecord::default()
            },
            DownloadRecord {
                source_url: "http://example.invalid/d.zip".to_string(),
                display_name: "delta.zip".to_string(),
                state: DownloadState::Interrupted,
                downloaded_bytes: 400,
                ..DownloadRecord::default()
            },
            DownloadRecord {
                source_url: "http://example.invalid/e.zip".to_string(),
                display_name: "echo.zip".to_string(),
                state: DownloadState::Queued,
                downloaded_bytes: 999,
                ..DownloadRecord::default()
            },
            DownloadRecord {
                source_url: "http://example.invalid/g.zip".to_string(),
                display_name: "foxtrot.zip".to_string(),
                state: DownloadState::Downloading,
                downloaded_bytes: 1,
                ..DownloadRecord::default()
            },
        ];
        let mut ids = HashMap::new();
        for mut record in seeds {
            repo.add(&mut record).expect("seed record");
            let id = record.id.expect("seed id");
            ids.insert(record.display_name.clone(), id);
        }
        ids
    }

    #[test]
    fn full_field_download_record_roundtrip() {
        let (dir, _db, repo) = test_repo("full_roundtrip");
        let mut record = DownloadRecord {
            id: None,
            source_url: "http://example.invalid/files/report.zip".to_string(),
            display_name: "My Report (2026).zip".to_string(),
            destination_path: dir.join("report.zip").to_string_lossy().into_owned(),
            state: DownloadState::Failed,
            downloaded_bytes: 482,
            expected_size: Some(16384),
            created_at: "2026-01-02T03:04:05+00:00".to_string(),
            completed_at: None,
            retry_count: 2,
            error_summary: Some("HTTP 500 after 3 attempts".to_string()),
            etag: Some("\"report-16384\"".to_string()),
            last_modified: Some("Mon, 02 Jan 2026 03:00:00 GMT".to_string()),
            checksum_sha256: Some("a".repeat(64)),
            checksum_verified: true,
            content_type: Some("application/zip".to_string()),
        };

        repo.add(&mut record).expect("add");
        let fetched = repo.get(record.id.expect("id")).expect("get").expect("row");

        assert_eq!(fetched, record);
    }

    #[test]
    fn add_blank_row_gets_id_and_created_at() {
        let (_dir, _db, repo) = test_repo("blank_row");
        let mut record = DownloadRecord {
            source_url: "http://example.invalid/a.zip".to_string(),
            ..DownloadRecord::default()
        };

        repo.add(&mut record).expect("add");
        let id = record.id.expect("id");
        let fetched = repo.get(id).expect("get").expect("row");

        assert_eq!(fetched.id, Some(id));
        assert!(!fetched.created_at.is_empty());
        assert_eq!(fetched.state, DownloadState::Queued);
    }

    #[test]
    fn settings_roundtrip_and_none_before_first_save() {
        let (dir, _db, repo) = test_repo("settings_roundtrip");
        assert!(repo.get_settings().expect("get").is_none());

        let mut extensions = BTreeSet::new();
        extensions.insert("zip".to_string());
        extensions.insert("pdf".to_string());
        let saved = Settings {
            default_destination_dir: dir.join("downloads").to_string_lossy().into_owned(),
            max_simultaneous: 7,
            connection_timeout: 11.5,
            retry_count: 5,
            retry_delay: 0.25,
            buffer_size: 32_768,
            bandwidth_limit_bps: 1_048_576,
            filename_collision: CollisionPolicy::Rename,
            notifications: false,
            theme: Theme::Light,
            user_agent: "TestAgent/1.0".to_string(),
            include_unknown_types: true,
            allowed_extensions: extensions,
            respect_rate_limit: false,
            allow_private_addresses: true,
        }
        .validated()
        .expect("valid settings");

        repo.save_settings(&saved).expect("save");
        let loaded = repo.get_settings().expect("get").expect("settings row");

        assert_eq!(
            loaded.default_destination_dir,
            saved.default_destination_dir
        );
        assert_eq!(loaded.max_simultaneous, saved.max_simultaneous);
        assert_eq!(loaded.connection_timeout, saved.connection_timeout);
        assert_eq!(loaded.retry_count, saved.retry_count);
        assert_eq!(loaded.retry_delay, saved.retry_delay);
        assert_eq!(loaded.buffer_size, saved.buffer_size);
        assert_eq!(loaded.bandwidth_limit_bps, saved.bandwidth_limit_bps);
        assert_eq!(loaded.filename_collision, saved.filename_collision);
        assert_eq!(loaded.notifications, saved.notifications);
        assert_eq!(loaded.theme, saved.theme);
        assert_eq!(loaded.user_agent, saved.user_agent);
        assert_eq!(loaded.include_unknown_types, saved.include_unknown_types);
        assert_eq!(loaded.allowed_extensions, saved.allowed_extensions);
        assert_eq!(loaded.respect_rate_limit, saved.respect_rate_limit);
        assert_eq!(
            loaded.allow_private_addresses,
            saved.allow_private_addresses
        );
    }

    #[test]
    fn recover_interrupted_clamps_to_part_size() {
        let (dir, _db, repo) = test_repo("recover_part");
        let destination = dir.join("big.zip");
        let mut record = DownloadRecord {
            source_url: "http://example.invalid/files/big.zip".to_string(),
            display_name: "big.zip".to_string(),
            destination_path: destination.to_string_lossy().into_owned(),
            state: DownloadState::Downloading,
            downloaded_bytes: 123,
            expected_size: Some(16384),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add");
        std::fs::write(format!("{}.part", destination.display()), vec![b'x'; 500])
            .expect("write part");

        let recovered = repo.recover_interrupted().expect("recover");
        let id = record.id.expect("id");
        assert_eq!(
            recovered.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![Some(id)]
        );
        assert_eq!(recovered[0].state, DownloadState::Interrupted);
        assert_eq!(recovered[0].downloaded_bytes, 500);
        let fetched = repo.get(id).expect("get").expect("row");
        assert_eq!(fetched.state, DownloadState::Interrupted);
        assert_eq!(fetched.downloaded_bytes, 500);

        assert!(repo.recover_interrupted().expect("recover").is_empty());
    }

    #[test]
    fn recover_interrupted_no_files_means_zero_bytes() {
        let (dir, _db, repo) = test_repo("recover_nofiles");
        let mut record = DownloadRecord {
            source_url: "http://example.invalid/files/gone.zip".to_string(),
            display_name: "gone.zip".to_string(),
            destination_path: dir.join("gone.zip").to_string_lossy().into_owned(),
            state: DownloadState::Downloading,
            downloaded_bytes: 123,
            expected_size: Some(1024),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add");

        let recovered = repo.recover_interrupted().expect("recover");
        assert_eq!(recovered[0].state, DownloadState::Interrupted);
        assert_ne!(recovered[0].state, DownloadState::Completed);
        assert_eq!(recovered[0].downloaded_bytes, 0);

        let id = record.id.expect("id");
        let fetched = repo.get(id).expect("get").expect("row");
        assert_eq!(fetched.state, DownloadState::Interrupted);
        assert_eq!(fetched.downloaded_bytes, 0);
        assert_eq!(fetched.completed_at, None);
    }

    #[test]
    fn recover_interrupted_complete_file_on_disk_marks_completed() {
        let (dir, _db, repo) = test_repo("recover_complete");
        let destination = dir.join("done.bin");
        std::fs::write(&destination, vec![b'z'; 2048]).expect("write final");
        let mut record = DownloadRecord {
            source_url: "http://example.invalid/files/data.bin".to_string(),
            display_name: "done.bin".to_string(),
            destination_path: destination.to_string_lossy().into_owned(),
            state: DownloadState::Downloading,
            downloaded_bytes: 100,
            expected_size: Some(2048),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add");

        let recovered = repo.recover_interrupted().expect("recover");
        assert_eq!(recovered[0].state, DownloadState::Completed);
        assert!(recovered[0].completed_at.is_some());
        assert_eq!(recovered[0].downloaded_bytes, 2048);

        let id = record.id.expect("id");
        let fetched = repo.get(id).expect("get").expect("row");
        assert_eq!(fetched.state, DownloadState::Completed);
        assert!(repo.recover_interrupted().expect("recover").is_empty());
    }

    #[test]
    fn recover_interrupted_skips_empty_database_and_settled_rows() {
        let (dir, _db, repo) = test_repo("recover_settled");
        let mut record = DownloadRecord {
            source_url: "http://example.invalid/f.zip".to_string(),
            display_name: "f.zip".to_string(),
            destination_path: dir.join("f.zip").to_string_lossy().into_owned(),
            state: DownloadState::Completed,
            downloaded_bytes: 10,
            expected_size: Some(10),
            completed_at: Some("2026-01-01T00:00:00+00:00".to_string()),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add");

        assert!(repo.recover_interrupted().expect("recover").is_empty());

        let fetched = repo.get(record.id.expect("id")).expect("get").expect("row");
        assert_eq!(fetched.state, DownloadState::Completed);
        assert_eq!(fetched.downloaded_bytes, 10);
    }

    #[test]
    fn history_excludes_active_rows() {
        let (_dir, _db, repo) = test_repo("history_active");
        seed_history(&repo);

        let rows = repo
            .history(HistoryQuery {
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("history");

        assert_eq!(
            HISTORY_STATES,
            [
                DownloadState::Cancelled,
                DownloadState::Completed,
                DownloadState::Failed,
                DownloadState::Interrupted,
            ]
        );
        let names: HashSet<&str> = rows.iter().map(|row| row.display_name.as_str()).collect();
        assert_eq!(
            names,
            HashSet::from(["alpha.zip", "bravo zip report", "charlie.zip", "delta.zip"])
        );
        assert!(SORTABLE_COLUMNS.contains(&"downloaded_bytes"));
        assert!(rows.iter().all(
            |row| row.state != DownloadState::Queued && row.state != DownloadState::Downloading
        ));
    }

    #[test]
    fn history_filter_by_state() {
        let (_dir, _db, repo) = test_repo("history_state");
        seed_history(&repo);

        let failed = repo
            .history(HistoryQuery {
                state: Some("failed".to_string()),
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("failed history");
        assert_eq!(
            failed
                .iter()
                .map(|row| row.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["bravo zip report"]
        );

        let completed = repo
            .history(HistoryQuery {
                state: Some("completed".to_string()),
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("completed history");
        assert_eq!(
            completed
                .iter()
                .map(|row| row.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha.zip"]
        );
    }

    #[test]
    fn history_search_substring() {
        let (_dir, _db, repo) = test_repo("history_search");
        let ids = seed_history(&repo);

        let rows = repo
            .history(HistoryQuery {
                search: Some("REPORT".to_string()),
                ..HistoryQuery::default()
            })
            .expect("search");

        assert_eq!(
            rows.iter()
                .map(|row| row.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["bravo zip report"]
        );
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![Some(ids["bravo zip report"])]
        );
    }

    #[test]
    fn history_sort_by_downloaded_bytes_desc() {
        let (_dir, _db, repo) = test_repo("history_sort");
        seed_history(&repo);

        let descending = repo
            .history(HistoryQuery {
                sort_by: "downloaded_bytes".to_string(),
                sort_dir: "DESC".to_string(),
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("descending");
        assert_eq!(
            descending
                .iter()
                .map(|row| row.downloaded_bytes)
                .collect::<Vec<_>>(),
            vec![400, 300, 200, 100]
        );

        let ascending = repo
            .history(HistoryQuery {
                sort_by: "downloaded_bytes".to_string(),
                sort_dir: "ASC".to_string(),
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("ascending");
        assert_eq!(
            ascending
                .iter()
                .map(|row| row.downloaded_bytes)
                .collect::<Vec<_>>(),
            vec![100, 200, 300, 400]
        );
    }

    #[test]
    fn history_sort_validation() {
        let (_dir, _db, repo) = test_repo("history_validation");
        seed_history(&repo);

        let error = repo
            .history(HistoryQuery {
                sort_by: "downloaded_bytes; DROP TABLE users".to_string(),
                ..HistoryQuery::default()
            })
            .expect_err("injection rejected");
        assert!(error.to_string().contains("sort_by must be one of"));

        let error = repo
            .history(HistoryQuery {
                sort_by: "not-a-column".to_string(),
                ..HistoryQuery::default()
            })
            .expect_err("unknown column rejected");
        assert!(error.to_string().contains("sort_by must be one of"));

        let error = repo
            .history(HistoryQuery {
                sort_dir: "sideways".to_string(),
                ..HistoryQuery::default()
            })
            .expect_err("bad direction rejected");
        assert!(error.to_string().contains("sort_dir must be ASC or DESC"));
    }

    #[test]
    fn clear_history_keep_files_preserves_disk() {
        let (dir, _db, repo) = test_repo("clear_keep");
        let destination = seed_completed_with_file(&dir, &repo, "keepme.zip");

        let removed = repo.clear_history(true).expect("clear");

        assert_eq!(removed, 1);
        assert!(
            repo.history(HistoryQuery {
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("history")
            .is_empty()
        );
        assert!(destination.is_file());
    }

    #[test]
    fn clear_history_without_keep_files_deletes_only_destination_files() {
        let (dir, _db, repo) = test_repo("clear_delete");
        let keep = seed_completed_with_file(&dir, &repo, "first.zip");
        let doomed = seed_completed_with_file(&dir, &repo, "second.zip");
        let sibling = dir.join("sibling.txt");
        std::fs::write(&sibling, "not a download").expect("write sibling");

        let removed = repo.clear_history(false).expect("clear");

        assert_eq!(removed, 2);
        assert!(
            repo.history(HistoryQuery {
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("history")
            .is_empty()
        );
        assert!(!keep.exists());
        assert!(!doomed.exists());
        assert!(sibling.is_file());
        assert_eq!(
            std::fs::read_to_string(&sibling).expect("read sibling"),
            "not a download"
        );
    }

    #[test]
    fn schema_init_is_idempotent_and_survives_reopen() {
        let dir = std::env::temp_dir().join("bdm-db-tests-reopen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("downloads.db");

        let id = {
            let db = Database::init_db(&path).expect("first init");
            Database::init_db(&path).expect("second init");
            let repo = DownloadRepository::new(Arc::new(db));
            let mut record = DownloadRecord {
                source_url: "http://example.invalid/reopen.zip".to_string(),
                ..DownloadRecord::default()
            };
            repo.add(&mut record).expect("add");
            record.id.expect("id")
        };

        let repo = DownloadRepository::new(Arc::new(Database::init_db(&path).expect("reopen")));
        let fetched = repo.get(id).expect("get").expect("row");
        assert_eq!(fetched.source_url, "http://example.invalid/reopen.zip");
    }

    #[test]
    fn in_memory_database_works() {
        let db = Database::open(":memory:").expect("open memory");
        db.ensure_schema().expect("schema");
        let repo = DownloadRepository::new(Arc::new(db));

        let mut record = DownloadRecord {
            source_url: "http://example.invalid/memory.zip".to_string(),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add");

        let fetched = repo.get(record.id.expect("id")).expect("get").expect("row");
        assert_eq!(fetched.source_url, "http://example.invalid/memory.zip");
    }

    #[test]
    fn duplicate_urls_are_allowed_and_find_by_url_returns_first() {
        let (_dir, _db, repo) = test_repo("duplicate_urls");
        let mut first = DownloadRecord {
            source_url: "http://example.invalid/dup.zip".to_string(),
            display_name: "first.zip".to_string(),
            ..DownloadRecord::default()
        };
        let mut second = DownloadRecord {
            source_url: "http://example.invalid/dup.zip".to_string(),
            display_name: "second.zip".to_string(),
            ..DownloadRecord::default()
        };
        repo.add(&mut first).expect("first");
        repo.add(&mut second).expect("second");

        assert_eq!(repo.list_all().expect("list").len(), 2);
        let found = repo
            .find_by_url("http://example.invalid/dup.zip")
            .expect("find")
            .expect("row");
        assert_eq!(found.id, first.id);
        assert_eq!(found.display_name, "first.zip");
    }

    #[test]
    fn update_and_delete_report_existence() {
        let (_dir, _db, repo) = test_repo("update_delete");
        let mut record = DownloadRecord {
            source_url: "http://example.invalid/u.zip".to_string(),
            display_name: "u.zip".to_string(),
            ..DownloadRecord::default()
        };
        repo.add(&mut record).expect("add");
        let id = record.id.expect("id");

        let mut missing = record.clone();
        missing.id = Some(999_999);
        assert!(!repo.update(&missing).expect("update missing"));
        assert!(!repo.delete(999_999).expect("delete missing"));

        repo.update_state(id, DownloadState::Paused)
            .expect("update state");
        let fetched = repo.get(id).expect("get").expect("row");
        assert_eq!(fetched.state, DownloadState::Paused);
        assert_eq!(fetched.source_url, record.source_url);
        assert_eq!(fetched.display_name, record.display_name);
        assert_eq!(fetched.created_at, record.created_at);
    }

    #[test]
    fn history_pagination_limit_offset() {
        let (_dir, _db, repo) = test_repo("history_pagination");
        let seeds = [
            ("p1.zip", "2026-01-01T00:00:01+00:00"),
            ("p2.zip", "2026-01-01T00:00:02+00:00"),
            ("p3.zip", "2026-01-01T00:00:03+00:00"),
            ("p4.zip", "2026-01-01T00:00:04+00:00"),
        ];
        for (name, created_at) in seeds {
            let mut record = DownloadRecord {
                source_url: format!("http://example.invalid/{name}"),
                display_name: name.to_string(),
                state: DownloadState::Completed,
                created_at: created_at.to_string(),
                downloaded_bytes: 1,
                expected_size: Some(1),
                ..DownloadRecord::default()
            };
            repo.add(&mut record).expect("add");
        }

        let page = repo
            .history(HistoryQuery {
                limit: 2,
                offset: 1,
                ..HistoryQuery::default()
            })
            .expect("page");
        assert_eq!(
            page.iter()
                .map(|row| row.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["p3.zip", "p2.zip"]
        );

        let all = repo
            .history(HistoryQuery {
                limit: 0,
                ..HistoryQuery::default()
            })
            .expect("all");
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn corrupt_settings_row_falls_back_to_none() {
        let (_dir, db, repo) = test_repo("corrupt_settings");
        db.execute("INSERT INTO settings (id, theme) VALUES (1, 'bogus')", &[])
            .expect("insert corrupt theme");
        assert!(repo.get_settings().expect("get").is_none());

        db.execute(
            "UPDATE settings SET theme = 'dark', allowed_extensions = 'not json' WHERE id = 1",
            &[],
        )
        .expect("update corrupt json");
        assert!(repo.get_settings().expect("get").is_none());
    }

    #[test]
    fn parent_dirs_created_for_nested_path() {
        let dir = std::env::temp_dir().join("bdm-db-tests-nested");
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("a").join("b").join("c").join("downloads.db");

        let db = Database::init_db(&nested).expect("nested init");

        assert!(nested.is_file());
        assert_eq!(db.path(), nested.to_string_lossy().as_ref());
    }
}

//! The persisted download record (one row of the downloads table).

use chrono::{SecondsFormat, Utc};

use super::download_status::DownloadState;

/// One download as persisted in the database and surfaced to the UI.
///
/// Field-for-field port of the Python DownloadRecord dataclass; the
/// storage layer maps these fields onto the downloads table columns.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadRecord {
    pub id: Option<i64>,
    pub source_url: String,
    pub display_name: String,
    pub destination_path: String,
    pub state: DownloadState,
    pub downloaded_bytes: u64,
    pub expected_size: Option<u64>,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub retry_count: u32,
    pub error_summary: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub checksum_sha256: Option<String>,
    pub checksum_verified: bool,
    pub content_type: Option<String>,
    /// Machine-readable failure class ([`super::diagnostic::FailureCategory::as_str`]),
    /// `None` for rows written before schema v2.
    pub failure_category: Option<String>,
}

impl Default for DownloadRecord {
    fn default() -> Self {
        Self {
            id: None,
            source_url: String::new(),
            display_name: String::new(),
            destination_path: String::new(),
            state: DownloadState::Queued,
            downloaded_bytes: 0,
            expected_size: None,
            created_at: String::new(),
            completed_at: None,
            retry_count: 0,
            error_summary: None,
            etag: None,
            last_modified: None,
            checksum_sha256: None,
            checksum_verified: false,
            content_type: None,
            failure_category: None,
        }
    }
}

impl DownloadRecord {
    /// A fresh queued record with a created_at timestamp.
    pub fn new_queued(
        source_url: impl Into<String>,
        display_name: impl Into<String>,
        destination_path: impl Into<String>,
    ) -> Self {
        Self {
            source_url: source_url.into(),
            display_name: display_name.into(),
            destination_path: destination_path.into(),
            created_at: now_iso(),
            ..Self::default()
        }
    }
}

/// Current UTC timestamp in the same ISO-8601 shape the Python app wrote
/// (YYYY-MM-DDTHH:MM:SS+00:00), so old rows stay comparable and sortable.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::download_status::DownloadState;

    #[test]
    fn new_queued_sets_state_and_timestamp() {
        let record =
            DownloadRecord::new_queued("http://example.com/a.zip", "a.zip", "C:/tmp/a.zip");
        assert_eq!(record.state, DownloadState::Queued);
        assert_eq!(record.source_url, "http://example.com/a.zip");
        assert!(record.created_at.contains("T"));
        assert!(record.id.is_none());
        assert_eq!(record.retry_count, 0);
        assert!(!record.checksum_verified);
    }

    #[test]
    fn now_iso_shape() {
        let stamp = now_iso();
        // 2026-10-25T12:34:56+00:00
        assert_eq!(stamp.len(), 25);
        assert!(stamp.ends_with("+00:00"));
        assert_eq!(&stamp[10..11], "T");
    }
}

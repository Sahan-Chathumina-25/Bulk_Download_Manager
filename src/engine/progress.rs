//! Throttled progress snapshots emitted while a download runs.

/// One progress observation for a running download.
///
/// Mirrors the Python ``DownloadProgress`` dataclass. `download_id` is the
/// persisted database id, or `-1` for an unsaved record (parity with the
/// legacy worker's ``record.id or -1`` fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadProgress {
    /// Database id of the download, or `-1` when it has not been persisted.
    pub download_id: i64,
    /// Bytes on disk so far (including any resumed prefix).
    pub downloaded_bytes: u64,
    /// Total size when the server told us (Content-Length / Content-Range).
    pub total_bytes: Option<u64>,
    /// Transfer speed in bytes per second for the current response.
    pub speed_bps: f64,
    /// Estimated seconds remaining, when a total and a positive speed exist.
    pub eta_seconds: Option<u64>,
}

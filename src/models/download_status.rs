//! Download lifecycle states and live progress snapshots.
//!
//! Port of the state-machine half of the Python database.models module.
//! The string form of each state is exactly what the SQLite state column
//! stores, so databases written by the Python app keep working.

use serde::{Deserialize, Serialize};

/// Lifecycle of one download row.
///
/// State machine (mirrors the Python DownloadState):
/// queued -> connecting -> downloading, then one of
/// completed | failed | cancelled | paused; interrupted marks rows that
/// were still running when the application last closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Connecting,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl DownloadState {
    /// The exact string stored in the SQLite state column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Connecting => "connecting",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse the database representation; unknown strings yield None.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "connecting" => Self::Connecting,
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }

    /// Terminal states never resume; the download is settled for good.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// States shown on the live Queue page.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Connecting | Self::Downloading | Self::Paused | Self::Interrupted
        )
    }
}

/// Terminal states (port of the Python TERMINAL_STATES constant).
pub const TERMINAL_STATES: [DownloadState; 3] = [
    DownloadState::Completed,
    DownloadState::Failed,
    DownloadState::Cancelled,
];

/// Throttled progress snapshot (~4/s) emitted by a running worker.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub download_id: i64,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: f64,
    pub eta_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_round_trip_matches_database_names() {
        for state in [
            DownloadState::Queued,
            DownloadState::Connecting,
            DownloadState::Downloading,
            DownloadState::Paused,
            DownloadState::Completed,
            DownloadState::Failed,
            DownloadState::Cancelled,
            DownloadState::Interrupted,
        ] {
            assert_eq!(DownloadState::parse(state.as_str()), Some(state));
        }
        assert_eq!(DownloadState::parse("bogus"), None);
    }

    #[test]
    fn terminal_and_live_classification() {
        assert!(DownloadState::Completed.is_terminal());
        assert!(DownloadState::Failed.is_terminal());
        assert!(DownloadState::Cancelled.is_terminal());
        assert!(!DownloadState::Interrupted.is_terminal());
        assert!(DownloadState::Queued.is_live());
        assert!(DownloadState::Paused.is_live());
        assert!(!DownloadState::Completed.is_live());
    }
}

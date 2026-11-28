//! Design tokens for the Dioxus shell.
//!
//! Colors live in `assets/main.css` so CSS media queries can follow the system
//! appearance. This module only maps states and severities to stable class
//! names, always paired with a visible text label in the UI.

use crate::app::state::Severity;
use crate::models::download_status::DownloadState;

/// Short, stable label for a download state; always shown next to its badge.
pub fn state_label(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => "Queued",
        DownloadState::Connecting => "Connecting",
        DownloadState::Downloading => "Downloading",
        DownloadState::Paused => "Paused",
        DownloadState::Completed => "Completed",
        DownloadState::Failed => "Failed",
        DownloadState::Cancelled => "Cancelled",
        DownloadState::Interrupted => "Interrupted",
    }
}

/// CSS class for a download state.
pub fn state_class(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Completed => "success",
        DownloadState::Connecting | DownloadState::Downloading => "accent",
        DownloadState::Queued | DownloadState::Interrupted | DownloadState::Cancelled => "muted",
        DownloadState::Paused => "warning",
        DownloadState::Failed => "danger",
    }
}

/// CSS class for a toast severity.
pub fn severity_class(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "accent",
        Severity::Success => "success",
        Severity::Warning => "warning",
        Severity::Error => "danger",
    }
}

/// True when a download state is actively occupying a worker slot.
pub fn is_active(state: DownloadState) -> bool {
    matches!(
        state,
        DownloadState::Queued
            | DownloadState::Connecting
            | DownloadState::Downloading
            | DownloadState::Paused
            | DownloadState::Interrupted
    )
}

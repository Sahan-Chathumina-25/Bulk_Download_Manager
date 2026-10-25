//! Error types shared across the application.

use thiserror::Error;

/// Errors that a single download can end with.
///
/// Mirrors the Python exception hierarchy: TransientHttp is retryable
/// (with an optional server Retry-After hint), Failed is permanent,
/// AuthRequired is never retried, and Paused/Cancelled are cooperative
/// stop signals rather than failures.
#[derive(Debug, Error)]
pub enum DownloadError {
    /// Retryable HTTP status (5xx, 429); retry_after carries the parsed
    /// Retry-After header in seconds when the server sent one.
    #[error("transient HTTP error {status}")]
    TransientHttp {
        status: u16,
        retry_after: Option<f64>,
    },

    /// Permanent failure with a user-actionable message.
    #[error("{0}")]
    Failed(String),

    /// 401/403 responses: retrying will not help.
    #[error("{0}")]
    AuthRequired(String),

    /// Cooperative pause signal (not an error condition).
    #[error("download paused")]
    Paused,

    /// Cooperative cancellation signal (not an error condition).
    #[error("download cancelled")]
    Cancelled,
}

impl DownloadError {
    /// Short one-line summary stored in the database error_summary column.
    pub fn summary(&self) -> String {
        match self {
            Self::TransientHttp { status, .. } => format!("transient HTTP error {status}"),
            Self::Failed(message) | Self::AuthRequired(message) => message.clone(),
            Self::Paused => "download paused".to_string(),
            Self::Cancelled => "download cancelled".to_string(),
        }
    }

    /// True when the operation may succeed after waiting and retrying.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::TransientHttp { .. })
    }
}

/// A URL that must not be dialed (bad scheme, private address, ...).
///
/// Port of the Python security.UnsafeUrlError (a ValueError subclass);
/// in Rust it is its own error type so callers can match on it precisely.
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct UnsafeUrlError(pub String);

impl UnsafeUrlError {
    /// Build from any message string.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

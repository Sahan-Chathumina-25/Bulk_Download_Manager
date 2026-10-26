//! Structured failure diagnostics for downloads that did not complete.
//!
//! The engine historically stored a single human-readable `error_summary`
//! string. That string is still written (tests and existing rows depend on
//! its exact wording), but a [`Diagnostic`] captures the machine-readable
//! shape of the same failure: which kind of problem it was, which host was
//! being contacted, how many redirect hops and retries were attempted, and
//! how long it took.
//!
//! # Privacy
//!
//! A diagnostic never stores a full URL. [`Diagnostic::host`] holds the bare
//! hostname only, and [`host_from_url`] strips credentials, the path, the
//! query and the port. Query strings frequently carry signed tokens, so they
//! must never reach the database or the logs.

use url::Url;

/// The broad class of problem a failed download hit.
///
/// Stored in the `downloads.failure_category` column as the string returned
/// by [`FailureCategory::as_str`] (stable, snake_case, safe for SQL and logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    /// Transport-level failure: DNS, connect, read or protocol error.
    Network,
    /// The connection or read timed out.
    Timeout,
    /// HTTP 401/403; the source needs credentials or browser verification.
    AuthRequired,
    /// HTTP 404/410; the file is gone.
    NotFound,
    /// HTTP 5xx or another retryable server-side status.
    ServerError,
    /// The server answered with an HTML page instead of the file.
    HtmlPage,
    /// A redirect target was refused by the URL safety policy.
    RedirectRejected,
    /// The destination resolved to a private/loopback/reserved address.
    PrivateAddress,
    /// The SHA-256 check did not match.
    ChecksumMismatch,
    /// A local disk operation failed.
    Filesystem,
    /// The destination already exists and the collision policy declined.
    Collision,
    /// The download was cancelled (by the user or by pausing without resume).
    Cancelled,
    /// Anything the engine could not classify more precisely.
    Unknown,
}

impl FailureCategory {
    /// Every variant, in declaration order (used by tests and the UI).
    pub const ALL: [Self; 13] = [
        Self::Network,
        Self::Timeout,
        Self::AuthRequired,
        Self::NotFound,
        Self::ServerError,
        Self::HtmlPage,
        Self::RedirectRejected,
        Self::PrivateAddress,
        Self::ChecksumMismatch,
        Self::Filesystem,
        Self::Collision,
        Self::Cancelled,
        Self::Unknown,
    ];

    /// Stable snake_case identifier persisted in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::AuthRequired => "auth_required",
            Self::NotFound => "not_found",
            Self::ServerError => "server_error",
            Self::HtmlPage => "html_page",
            Self::RedirectRejected => "redirect_rejected",
            Self::PrivateAddress => "private_address",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::Filesystem => "filesystem",
            Self::Collision => "collision",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored identifier; unknown values are `None` (never an error),
    /// so a row written by a newer build stays readable.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| category.as_str() == value)
    }

    /// One-sentence, user-facing explanation of the category.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Network => "A network error occurred",
            Self::Timeout => "The connection timed out",
            Self::AuthRequired => "Access requires browser verification",
            Self::NotFound => "The file was not found on the server",
            Self::ServerError => "The remote server reported an error",
            Self::HtmlPage => "The server returned an HTML page instead of the expected file",
            Self::RedirectRejected => "Redirect rejected by security policy",
            Self::PrivateAddress => "Destination resolved to a prohibited address",
            Self::ChecksumMismatch => "The downloaded file failed its integrity check",
            Self::Filesystem => "A local file error occurred",
            Self::Collision => "The destination file already exists",
            Self::Cancelled => "The download was cancelled",
            Self::Unknown => "The download failed",
        }
    }
}

/// Machine-readable context for one failed download.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// Repository id, when the record has been persisted.
    pub download_id: Option<i64>,
    /// Bare hostname of the URL that was being contacted when it failed.
    pub host: String,
    /// Redirect hops followed before the failure.
    pub redirect_count: u32,
    /// Last HTTP status seen, when a response was received.
    pub status: Option<u16>,
    /// Response `Content-Type`, when a response was received.
    pub content_type: Option<String>,
    /// Classified failure kind.
    pub category: FailureCategory,
    /// Retries attempted for this download.
    pub retries: u32,
    /// Wall-clock time spent before the failure.
    pub elapsed_seconds: f64,
}

impl Diagnostic {
    /// Compact, single-line rendering for tooltips and detail panes.
    ///
    /// Example: `host=example.com; status=404; redirects=0; retries=2;
    /// elapsed=1.3s`. Absent optional fields are omitted entirely. A
    /// non-positive `elapsed_seconds` means "not measured" (the GUI rebuilds
    /// diagnostics from persisted columns only) and is omitted too.
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::with_capacity(6);
        if !self.host.is_empty() {
            parts.push(format!("host={}", self.host));
        }
        if let Some(status) = self.status {
            parts.push(format!("status={status}"));
        }
        if let Some(content_type) = self.content_type.as_deref()
            && !content_type.is_empty()
        {
            parts.push(format!("content_type={content_type}"));
        }
        parts.push(format!("redirects={}", self.redirect_count));
        parts.push(format!("retries={}", self.retries));
        if self.elapsed_seconds > 0.0 {
            parts.push(format!("elapsed={:.1}s", self.elapsed_seconds));
        }
        parts.join("; ")
    }
}

/// Extract the bare hostname from `raw`, or `""` when it cannot be parsed.
///
/// Credentials, port, path and query are dropped so a signed URL cannot leak
/// into the database or a log line.
pub fn host_from_url(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_identifiers_roundtrip() {
        for category in FailureCategory::ALL {
            let stored = category.as_str();
            assert_eq!(FailureCategory::parse(stored), Some(category));
            assert!(!stored.is_empty());
            assert!(
                stored
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "unstable identifier: {stored:?}"
            );
        }
    }

    #[test]
    fn unknown_category_identifier_is_none() {
        assert_eq!(FailureCategory::parse("made_up"), None);
        assert_eq!(FailureCategory::parse(""), None);
        assert_eq!(FailureCategory::parse("HTML_PAGE"), None);
    }

    #[test]
    fn every_category_has_a_distinct_headline() {
        let mut seen = Vec::new();
        for category in FailureCategory::ALL {
            let headline = category.headline();
            assert!(!headline.is_empty());
            assert!(
                !seen.contains(&headline),
                "duplicate headline: {headline:?}"
            );
            seen.push(headline);
        }
    }

    #[test]
    fn summary_line_omits_absent_fields() {
        let diagnostic = Diagnostic {
            download_id: Some(7),
            host: "example.com".to_string(),
            redirect_count: 2,
            status: None,
            content_type: None,
            category: FailureCategory::Timeout,
            retries: 1,
            elapsed_seconds: 12.34,
        };

        assert_eq!(
            diagnostic.summary_line(),
            "host=example.com; redirects=2; retries=1; elapsed=12.3s"
        );
    }

    #[test]
    fn summary_line_includes_status_and_content_type_when_known() {
        let diagnostic = Diagnostic {
            download_id: None,
            host: "cdn.example.org".to_string(),
            redirect_count: 0,
            status: Some(404),
            content_type: Some("text/html; charset=utf-8".to_string()),
            category: FailureCategory::NotFound,
            retries: 3,
            elapsed_seconds: 0.4,
        };

        assert_eq!(
            diagnostic.summary_line(),
            "host=cdn.example.org; status=404; content_type=text/html; charset=utf-8; \
             redirects=0; retries=3; elapsed=0.4s"
        );
    }

    #[test]
    fn summary_line_omits_unmeasured_elapsed() {
        // The GUI rebuilds a diagnostic from persisted columns, which do not
        // include timing, so a zero elapsed time must not claim "0.0s".
        let diagnostic = Diagnostic {
            download_id: Some(3),
            host: "example.com".to_string(),
            redirect_count: 0,
            status: None,
            content_type: Some("text/html".to_string()),
            category: FailureCategory::HtmlPage,
            retries: 2,
            elapsed_seconds: 0.0,
        };

        assert_eq!(
            diagnostic.summary_line(),
            "host=example.com; content_type=text/html; redirects=0; retries=2"
        );
    }

    #[test]
    fn host_strips_credentials_port_path_and_query() {
        assert_eq!(
            host_from_url("https://user:secret@Example.COM:8443/files/a.zip?token=abc#frag"),
            "example.com"
        );
        assert_eq!(host_from_url("http://192.0.2.10:8080/page"), "192.0.2.10");
    }

    #[test]
    fn host_of_unparseable_input_is_empty() {
        assert_eq!(host_from_url(""), "");
        assert_eq!(host_from_url("not a url"), "");
        assert_eq!(host_from_url("ftp://example.com/x"), "example.com");
    }
}

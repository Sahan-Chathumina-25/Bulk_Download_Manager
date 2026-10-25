//! reqwest client construction for the download engine.
//!
//! # Redirects
//!
//! Automatic redirects are disabled (`Policy::none()`): the worker follows
//! them manually so every hop passes through
//! [`crate::services::validation::validate_redirect`] (scheme, private
//! address, and https-to-http downgrade checks).
//!
//! # DNS-rebinding limitation (not solved in this phase)
//!
//! [`crate::services::validation::validate_url_for_network`] resolves the
//! hostname and screens the resolved addresses at validation time, but the
//! TCP connection is **not pinned** to those validated IPs. A hostile DNS
//! resolver can answer validation with a public address and the connection
//! with a private one (classic rebinding attack). Re-validating every request
//! and every redirect shrinks the window but does not close it. The planned
//! future fix is a custom [`reqwest::dns::Resolve`] implementation wired in
//! through `ClientBuilder::dns_resolver` that only dials the addresses the
//! validator approved. It is deliberately **not** implemented in Phase 4.
//!
//! # Credentials
//!
//! The client never attaches credentials, cookies, or Authorization headers.
//! Requests carry only `User-Agent`, `Accept`, `Accept-Encoding: identity`,
//! `Range`, and `If-Range`, so nothing sensitive can leak across a redirect
//! origin (and each hop is revalidated anyway).

use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};

use crate::config::Settings;

/// Build the shared HTTP client for one worker.
///
/// Per-operation timeouts are used (connect plus a per-read idle timeout)
/// instead of a single total request timeout: a total timeout would abort
/// large downloads that are still making progress. User-Agent is attached
/// per request by the worker (matching the legacy client), never here.
pub fn build_client(settings: &Settings) -> Result<reqwest::Client, reqwest::Error> {
    let seconds = if settings.connection_timeout.is_finite() && settings.connection_timeout > 0.0 {
        settings.connection_timeout
    } else {
        30.0
    };
    let timeout = Duration::from_secs_f64(seconds);

    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .default_headers(headers)
        .build()
}

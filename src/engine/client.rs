//! reqwest client construction for the download engine.
//!
//! # Redirects
//!
//! Automatic redirects are disabled (`Policy::none()`): the worker follows
//! them manually so every hop passes through
//! [`crate::services::validation::validate_redirect`] (scheme, private
//! address, and https-to-http downgrade checks).
//!
//! # DNS rebinding
//!
//! [`crate::services::validation::validate_url_for_network`] resolves the
//! hostname and screens the addresses at validation time. Without further
//! action a hostile resolver could answer validation with a public address and
//! the connection with a private one (classic rebinding).
//!
//! That gap is now closed for the download path: the worker installs a
//! [`PinningResolver`] on the client and pins every validated host to the
//! exact addresses the validator approved
//! ([`crate::services::validation::ValidatedUrl::resolved_ips`]). reqwest's
//! connector therefore only ever dials an approved address — a second lookup
//! for a pinned host never happens. The pins are refreshed after every
//! manually followed redirect, so each hop is validated *and* dialled on the
//! addresses that validation saw.
//!
//! Pinning changes only address selection: the URL, the `Host` header, TLS SNI
//! and certificate verification still use the hostname, so pinning an address
//! cannot break virtual hosting or certificate checks.
//!
//! Honest remaining caveats:
//!
//! - Hosts that are *not* pinned still fall back to the system resolver
//!   (screening still applies, but the lookup happens at connect time).
//! - Link discovery ([`crate::services::link_discovery`]) validates every
//!   candidate but does **not** pin; it performs no downloads, and a scan can
//!   only read a page or a HEAD response.
//! - A pin has no expiry: callers must re-pin on redirect (the worker does)
//!   and drop the resolver when the download ends.
//!
//! # Credentials
//!
//! The client never attaches credentials, cookies, or Authorization headers.
//! Requests carry only `User-Agent`, `Accept`, `Accept-Encoding: identity`,
//! `Range`, and `If-Range`, so nothing sensitive can leak across a redirect
//! origin (and each hop is revalidated anyway).

use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::Resolve;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};

use crate::config::Settings;

/// Build the shared HTTP client for one worker.
///
/// Per-operation timeouts are used (connect plus a per-read idle timeout)
/// instead of a single total request timeout: a total timeout would abort
/// large downloads that are still making progress. User-Agent is attached
/// per request by the worker (matching the legacy client), never here.
///
/// `resolver` installs a custom DNS resolver; pass the worker's
/// [`super::resolver::PinningResolver`] to restrict connections to validated
/// addresses, or `None` to keep reqwest's default system resolver.
pub fn build_client_with_resolver(
    settings: &Settings,
    resolver: Option<Arc<dyn Resolve>>,
) -> Result<reqwest::Client, reqwest::Error> {
    let seconds = if settings.connection_timeout.is_finite() && settings.connection_timeout > 0.0 {
        settings.connection_timeout
    } else {
        30.0
    };
    let timeout = Duration::from_secs_f64(seconds);

    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .default_headers(headers);
    if let Some(resolver) = resolver {
        builder = builder.dns_resolver(resolver);
    }
    builder.build()
}

/// Build the shared HTTP client for one worker using the system resolver.
pub fn build_client(settings: &Settings) -> Result<reqwest::Client, reqwest::Error> {
    build_client_with_resolver(settings, None)
}

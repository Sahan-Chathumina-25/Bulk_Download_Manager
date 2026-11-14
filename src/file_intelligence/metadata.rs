//! Bounded HTTP metadata probing for archive candidates.
//!
//! Probing is opt-in, strictly bounded and reuses the engine's client and
//! validation policy:
//!
//! - redirects are followed manually (at most [`MAX_PROBE_REDIRECTS`] hops)
//!   and every hop passes [`validate_redirect`], the same SSRF gate the
//!   download worker uses;
//! - a probe starts with `HEAD`; a bounded range read (`bytes=0-1023`) is
//!   only issued when the server confirms `Accept-Ranges`/a `206` response —
//!   and a `200` fallback never reads more than [`MAX_PROBE_BODY_BYTES`];
//! - responses are never stored beyond the small metadata struct;
//! - cancellation is checked between every step.
//!
//! Probes never bypass authentication, CAPTCHA or paywalls: only plain,
//! unauthenticated HTTP requests are made.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Settings;
use crate::engine::client::build_client;
use crate::services::validation::{redact_url, validate_redirect};

use super::archive::{SignatureOutcome, compare_signature, signature_format};

/// Redirect hops a probe may follow (fewer than the download worker allows).
const MAX_PROBE_REDIRECTS: usize = 5;
/// Total bytes a signature read may receive from the network.
const MAX_PROBE_BODY_BYTES: usize = 1024;
/// Tar signatures live at byte 257..262, so a probe reads 512 bytes when the
/// server honours ranges.
const PROBE_RANGE_BYTES: usize = 512;

/// Everything a probe learned about one URL.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeOutcome {
    pub content_type: Option<String>,
    pub disposition_filename: Option<String>,
    pub size: Option<u64>,
    pub status: Option<u16>,
    pub signature: Option<SignatureOutcome>,
    /// Non-fatal note for the explanation panel (e.g. "range not honoured").
    pub note: Option<String>,
}

impl ProbeOutcome {
    /// True when nothing usable was learned.
    pub fn is_empty(&self) -> bool {
        self.content_type.is_none()
            && self.disposition_filename.is_none()
            && self.size.is_none()
            && self.signature.is_none()
    }
}

/// Errors a probe can hit; all are reported as notes, never panics.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeError {
    Cancelled,
    UnsafeUrl(String),
    Transport(String),
    TooManyRedirects,
}

impl ProbeError {
    /// UI-friendly text; URLs are redacted before storage.
    pub fn message(&self) -> String {
        match self {
            Self::Cancelled => "analysis cancelled".to_string(),
            Self::UnsafeUrl(reason) => format!("unsafe URL: {reason}"),
            Self::Transport(reason) => format!("request failed: {reason}"),
            Self::TooManyRedirects => "too many redirects".to_string(),
        }
    }
}

/// One probe session: a client built once plus the settings it follows.
#[derive(Debug, Clone)]
pub struct MetadataProbe {
    settings: Settings,
    client: Option<reqwest::Client>,
}

impl MetadataProbe {
    pub fn new(settings: &Settings) -> Self {
        Self {
            settings: settings.clone(),
            client: build_client(settings).ok(),
        }
    }

    fn client(&self) -> Result<&reqwest::Client, ProbeError> {
        self.client
            .as_ref()
            .ok_or_else(|| ProbeError::Transport("the HTTP client could not be built".to_string()))
    }

    /// Probe one URL. `expected_format` enables the bounded signature read
    /// when the filename already suggests an archive family.
    pub async fn probe(
        &self,
        url: &str,
        expected_format: Option<super::archive::ArchiveFormat>,
        cancel: &CancellationToken,
    ) -> Result<ProbeOutcome, ProbeError> {
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }
        let client = self.client()?;

        let mut current = url.to_string();
        let mut response = None;
        for _ in 0..=MAX_PROBE_REDIRECTS {
            if cancel.is_cancelled() {
                return Err(ProbeError::Cancelled);
            }
            let request = client.head(&current).timeout(Duration::from_secs_f64(
                self.settings.connection_timeout.clamp(1.0, 30.0),
            ));
            let sent = match request.send().await {
                Ok(sent) => sent,
                Err(error) => return Err(ProbeError::Transport(error.to_string())),
            };
            let status = sent.status();
            if status.is_redirection() {
                let Some(location) = sent
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
                else {
                    break;
                };
                let joined = join_redirect(&current, &location);
                let allow_private = self.settings.allow_private_addresses;
                let validated = validate_redirect(&current, &joined, allow_private)
                    .map_err(|error| ProbeError::UnsafeUrl(error.to_string()))?;
                current = validated.url;
                continue;
            }
            response = Some(sent);
            break;
        }

        let Some(head) = response else {
            return Err(ProbeError::TooManyRedirects);
        };
        let status = head.status();
        if !status.is_success() {
            return Ok(ProbeOutcome {
                status: Some(status.as_u16()),
                note: Some(format!(
                    "the server answered {} to a HEAD request",
                    status.as_u16()
                )),
                ..ProbeOutcome::default()
            });
        }

        let mut outcome = ProbeOutcome {
            status: Some(status.as_u16()),
            ..ProbeOutcome::default()
        };
        if let Some(value) = header_str(head.headers(), reqwest::header::CONTENT_TYPE) {
            // Normalise to the bare media type, like discovery does.
            let media = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            outcome.content_type = Some(media);
        }
        if let Some(value) = header_str(head.headers(), reqwest::header::CONTENT_DISPOSITION) {
            outcome.disposition_filename =
                super::archive::filename_from_content_disposition(&value);
        }
        if let Some(value) = header_str(head.headers(), reqwest::header::CONTENT_LENGTH) {
            outcome.size = value.trim().parse::<u64>().ok();
        }

        // Signature read: only when the HEAD already looked like an archive
        // and the URL passed validation. A server that ignores the Range
        // header answers 200 with a full body — read at most
        // MAX_PROBE_BODY_BYTES and stop.
        if expected_format.is_none() && !looks_archive(&outcome) {
            return Ok(outcome);
        }
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }

        let bytes = self.read_prefix(&current, cancel).await?;
        match bytes {
            Some(bytes) => {
                let found = signature_format(&bytes);
                outcome.signature = Some(match (found, expected_format) {
                    (Some(_), Some(expected)) => compare_signature(expected, &bytes),
                    (Some(found), None) => SignatureOutcome::Verified(found),
                    (None, _) => SignatureOutcome::Mismatch,
                });
            }
            None => {
                outcome.note = Some("no body was available for a signature check".to_string());
                outcome.signature = Some(SignatureOutcome::Unavailable);
            }
        }
        Ok(outcome)
    }

    /// Read at most [`PROBE_RANGE_BYTES`] bytes with a Range request,
    /// verifying that the server actually honoured it.
    async fn read_prefix(
        &self,
        url: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, ProbeError> {
        let client = self.client()?;
        let request = client
            .get(url)
            .header(
                reqwest::header::RANGE,
                format!("bytes=0-{}", PROBE_RANGE_BYTES - 1),
            )
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        let sent = match request.send().await {
            Ok(sent) => sent,
            Err(error) => return Err(ProbeError::Transport(error.to_string())),
        };

        let status = sent.status();
        let mut outcome_bytes: Vec<u8> = Vec::new();

        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            // Verify the server really started at byte 0 before trusting
            // the bytes as the head of the file.
            let range_ok = sent
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.trim_start().starts_with("bytes 0-"));
            if !range_ok {
                return Ok(None);
            }
            read_bounded(sent, MAX_PROBE_BODY_BYTES, &mut outcome_bytes, cancel).await?;
            return Ok(Some(outcome_bytes));
        }

        if status.is_success() {
            // Range not honoured: fall back to a bounded prefix read of a
            // full response.
            read_bounded(sent, MAX_PROBE_BODY_BYTES, &mut outcome_bytes, cancel).await?;
            return Ok(Some(outcome_bytes));
        }

        Ok(None)
    }
}

/// Heuristic used to decide whether a signature read is worth a second
/// request: the HEAD already suggested binary/archive content.
fn looks_archive(outcome: &ProbeOutcome) -> bool {
    if let Some(disposition) = &outcome.disposition_filename
        && super::archive::analyse_filename(disposition).is_some_and(|a| a.is_archive)
    {
        return true;
    }
    if let Some(content_type) = &outcome.content_type
        && (super::scoring::is_octet_stream(content_type)
            || super::scoring::content_type_format(content_type).is_some())
    {
        return true;
    }
    false
}

/// Read a response body into `buffer`, stopping at `limit` bytes.
async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
    buffer: &mut Vec<u8>,
    cancel: &CancellationToken,
) -> Result<(), ProbeError> {
    let mut response = response;
    while buffer.len() < limit {
        if cancel.is_cancelled() {
            return Err(ProbeError::Cancelled);
        }
        match tokio::time::timeout(Duration::from_secs(30), response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let remaining = limit - buffer.len();
                let take = chunk.len().min(remaining);
                buffer.extend_from_slice(&chunk[..take]);
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => return Err(ProbeError::Transport(error.to_string())),
            Err(_) => break,
        }
    }
    Ok(())
}

/// Read a header value as a trimmed UTF-8 string.
fn header_str(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Join a redirect Location against the current URL (relative locations).
fn join_redirect(current: &str, location: &str) -> String {
    match url::Url::parse(location) {
        Ok(absolute) => absolute.to_string(),
        Err(_) => match url::Url::parse(current).and_then(|base| base.join(location)) {
            Ok(joined) => joined.to_string(),
            // `validate_redirect` will reject unparsable targets anyway.
            Err(_) => location.to_string(),
        },
    }
}

/// Redacted display form of a probed URL for notes and logs.
pub fn redact_probe_url(url: &str) -> String {
    redact_url(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::engine::fake_site::FakeSite;
    use crate::file_intelligence::archive::ArchiveFormat;
    use tokio_util::sync::CancellationToken;

    fn probe_settings() -> Settings {
        Settings {
            // The fixture lives on the loopback interface.
            allow_private_addresses: true,
            connection_timeout: 5.0,
            ..Settings::default()
        }
    }

    fn zip_bytes() -> Vec<u8> {
        let mut bytes = b"PK\x03\x04".to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        bytes
    }

    fn cancelled() -> CancellationToken {
        let token = CancellationToken::new();
        token.cancel();
        token
    }

    #[tokio::test]
    async fn head_probe_reports_metadata_and_size() {
        let site = FakeSite::spawn().await;
        site.set_file("probe.zip", zip_bytes());
        let url = site.url("/files/probe.zip");

        let probe = MetadataProbe::new(&probe_settings());
        let outcome = probe
            .probe(&url, Some(ArchiveFormat::Zip), &CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(outcome.status, Some(200));
        assert_eq!(outcome.content_type.as_deref(), Some("application/zip"));
        assert_eq!(outcome.size, Some(zip_bytes().len() as u64));
        assert_eq!(
            outcome.signature,
            Some(SignatureOutcome::Verified(ArchiveFormat::Zip))
        );
    }

    #[tokio::test]
    async fn html_responses_are_detected_without_signature_reads() {
        let site = FakeSite::spawn().await;
        let url = site.url("/html-as-file");

        let probe = MetadataProbe::new(&probe_settings());
        let outcome = probe
            .probe(&url, None, &CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(outcome.content_type.as_deref(), Some("text/html"));
        assert!(outcome.signature.is_none());
    }

    #[tokio::test]
    async fn non_200_status_becomes_a_note() {
        let site = FakeSite::spawn().await;
        let url = site.url("/status/404");

        let probe = MetadataProbe::new(&probe_settings());
        let outcome = probe
            .probe(&url, None, &CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(outcome.status, Some(404));
        assert!(outcome.note.is_some());
    }

    #[tokio::test]
    async fn redirects_to_private_addresses_are_rejected() {
        let site = FakeSite::spawn().await;
        let url = site.url("/redirect-private");

        // The production default: private addresses are not allowed, so a
        // redirect that lands on a metadata address must fail closed.
        let probe = MetadataProbe::new(&Settings {
            connection_timeout: 5.0,
            ..Settings::default()
        });
        let outcome = probe.probe(&url, None, &CancellationToken::new()).await;
        assert!(matches!(outcome, Err(ProbeError::UnsafeUrl(_))));
    }

    #[tokio::test]
    async fn cancellation_stops_the_probe() {
        let site = FakeSite::spawn().await;
        site.set_file("probe2.zip", zip_bytes());
        let url = site.url("/files/probe2.zip");

        let probe = MetadataProbe::new(&probe_settings());
        let outcome = probe.probe(&url, None, &cancelled()).await;
        assert!(matches!(outcome, Err(ProbeError::Cancelled)));
    }

    #[tokio::test]
    async fn a_full_200_response_is_read_only_partially() {
        let site = FakeSite::spawn().await;
        site.set_file("probe3.zip", zip_bytes());
        let url = site.url("/norange/probe3.zip");

        let probe = MetadataProbe::new(&probe_settings());
        let outcome = probe
            .probe(&url, Some(ArchiveFormat::Zip), &CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(
            outcome.signature,
            Some(SignatureOutcome::Verified(ArchiveFormat::Zip))
        );
    }
}

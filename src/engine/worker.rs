//! Single-download streaming engine.
//!
//! Faithful port of the legacy Python ``src.core.download_worker``:
//!
//! - manual redirect loop (max [`MAX_REDIRECT_HOPS`], every hop re-validated
//!   for SSRF safety and https-to-http downgrades),
//! - HTTP 206 resume with `Range` + `If-Range`, restart-from-zero when the
//!   server ignores the range or the validators changed (never append across
//!   a gap),
//! - throttled progress events (~4/s) with speed and ETA,
//! - retry/backoff delegated to [`super::retry`],
//! - HTML-instead-of-file detection, Content-Length verification, and
//!   optional SHA-256 verification,
//! - cooperative cancel/pause through a [`CancellationToken`] and an
//!   [`AtomicBool`].
//!
//! `DownloadWorker::run` never propagates pause/cancel/download failures: it
//! always settles the record and persists it when a repository id is present.
//!
//! # Divergences from the legacy worker
//!
//! - Disk I/O failures are classified as **permanent** with a
//!   `filesystem error: ...` prefix. The legacy code mislabeled them as
//!   `network failure` or `internal error`.
//! - Checksum verification on a resumed transfer hashes the **entire**
//!   `.part` file, not just the bytes of the current response.
//! - A pre-rename `.part` size check runs before the atomic publish.
//! - Successful completion clears any stale `error_summary`.
//! - Pausing a server without range support behaves like cancel (legacy
//!   parity), documented at [`DownloadWorker::check_user_stop`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use reqwest::Response;
use reqwest::header::{ACCEPT_ENCODING, IF_RANGE, RANGE, USER_AGENT};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{CollisionPolicy, Settings};
use crate::database::repository::DownloadRepository;
use crate::errors::UnsafeUrlError;
use crate::models::diagnostic::{Diagnostic, FailureCategory, host_from_url};
use crate::models::download_item::{DownloadRecord, now_iso};
use crate::models::download_status::DownloadState;
use crate::services::{file_naming, validation};

use super::client::build_client_with_resolver;
use super::progress::DownloadProgress;
use super::resolver::PinningResolver;
use super::retry::{
    MAX_RETRY_AFTER_SECONDS, RetryOutcome, RetryPolicy, TRANSIENT_STATUS_CODES, parse_retry_after,
};

/// Maximum number of redirect hops (the worker makes at most one more
/// request than this before declaring a loop).
pub const MAX_REDIRECT_HOPS: usize = 10;

/// Minimum interval between progress events (~4 per second).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Upper bound used when deriving the policy from settings.
const MAX_DELAY_SECONDS: f64 = 60.0;

/// Statuses that represent a redirect the worker follows manually.
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// User-facing message when the server serves an HTML page instead of a file.
const HTML_ERROR: &str = "the server returned an HTML page instead of the requested file; \
     the link may be a landing/login page — open it in a browser";

/// Appended to a 404/410 whose body is an HTML page: file lockers answer a
/// gone/expired link with their landing page, which needs a browser to proceed.
const NOT_FOUND_HTML_HINT: &str = "; the server responded with an HTML page — the link may be \
     expired, removed, or require browser interaction";

/// 401: the server demands credentials the app has no way to supply.
const AUTH_REQUIRED_MESSAGE: &str = "server requires authentication (HTTP 401)";

/// 403 whose body is HTML: bot protection / browser verification. The app has
/// no credentials or cookies feature, so do not suggest configuring any.
const AUTH_BLOCKED_HTML_MESSAGE: &str = "the server refused automated access (HTTP 403); the site \
     requires browser verification — open the link in a browser";

/// 403 with a non-HTML body: the resource may sit behind credentials.
const AUTH_BLOCKED_MESSAGE: &str = "the server refused access (HTTP 403); the resource may require \
     credentials the app does not provide";

/// A Cloudflare/bot-verification challenge page (managed challenge, DDoS-Guard,
/// ...) served instead of the file. Such pages arrive with 200, 403 or 503, so
/// the message must not name a status. The app cannot complete an interactive
/// challenge: say so plainly and point the user at a browser, never imply the
/// download can bypass the check.
const BROWSER_CHALLENGE_MESSAGE: &str = "the site requires browser verification — automated \
     download is not possible; open the link in a browser and, if it offers a direct file link, \
     copy that into Add Downloads";

/// Upper bound on the response prefix inspected for a challenge page.
const CHALLENGE_SNIFF_LIMIT: usize = 8 * 1024;

/// Body markers specific enough to identify an interactive verification page.
const CHALLENGE_BODY_MARKERS: [&str; 5] = [
    "challenge-platform",
    "challenges.cloudflare.com",
    "cf-chl",
    "cf-browser-verification",
    "ddos-guard",
];

/// Errors the engine can settle a download with.
///
/// `TransientHttp` and `Network` are retryable; everything else is permanent.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A retryable HTTP status, with an optional parsed Retry-After hint.
    #[error("transient HTTP error {status}")]
    TransientHttp {
        status: u16,
        retry_after: Option<f64>,
    },

    /// 401/403: retrying will not help. `message` is the complete user-facing
    /// explanation, without suggesting credentials the app cannot supply.
    #[error("{message}")]
    AuthRequired { message: &'static str },

    /// 404/410: the file is gone. `hint` is empty unless the server answered
    /// with an HTML page (expired/removed link or a JS-gated locker).
    #[error("file not found on server (HTTP {status}){hint}")]
    NotFound { status: u16, hint: &'static str },

    /// Any other non-2xx status.
    #[error("unexpected HTTP status {0}")]
    UnexpectedStatus(u16),

    /// A transport-level failure (DNS, connect, read, protocol).
    #[error("{0}")]
    Network(#[from] reqwest::Error),

    /// A permanent, user-actionable failure.
    #[error("{0}")]
    Failed(String),

    /// The URL (or a redirect target) failed network-safety validation.
    #[error("unsafe URL refused: {0}")]
    UnsafeUrl(#[from] UnsafeUrlError),
}

impl EngineError {
    /// True when the failure may succeed after a delay.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::TransientHttp { .. } | Self::Network(_))
    }

    fn outcome(&self) -> RetryOutcome {
        match self {
            Self::TransientHttp { retry_after, .. } => RetryOutcome::Transient {
                retry_after: *retry_after,
            },
            Self::Network(_) => RetryOutcome::Network,
            _ => RetryOutcome::Permanent,
        }
    }

    fn failure_message(&self) -> String {
        match self {
            Self::TransientHttp { .. } | Self::Network(_) => {
                format!("network failure: {}", self.sanitized_message())
            }
            _ => self.to_string(),
        }
    }

    /// Display text with any request URL that `reqwest` embeds removed.
    ///
    /// Only used for logging and the persisted `error_summary`; the structured
    /// error keeps the original text for callers that match on it.
    fn sanitized_message(&self) -> String {
        match self {
            Self::Network(error) => sanitize_transport_error(&error.to_string()),
            _ => self.to_string(),
        }
    }

    /// Structured failure class for the diagnostics column.
    fn category(&self) -> FailureCategory {
        match self {
            Self::TransientHttp { status, .. } | Self::UnexpectedStatus(status) => {
                classify_status(*status)
            }
            Self::AuthRequired { .. } => FailureCategory::AuthRequired,
            Self::NotFound { .. } => FailureCategory::NotFound,
            Self::Network(error) if error.is_timeout() => FailureCategory::Timeout,
            Self::Network(_) => FailureCategory::Network,
            Self::Failed(message) => classify_failure_message(message),
            Self::UnsafeUrl(error) => {
                if is_private_address_message(&error.0) {
                    FailureCategory::PrivateAddress
                } else {
                    FailureCategory::Unknown
                }
            }
        }
    }
}

/// Events a running worker emits (progress snapshots and state changes).
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    Progress(DownloadProgress),
    StateChanged(Box<DownloadRecord>),
}

/// Internal control flow for one lifecycle pass.
#[derive(Debug)]
enum RunError {
    Engine(EngineError),
    Io(std::io::Error),
    /// The response cannot extend `.part`; truncate and request again.
    FreshRestart,
    /// The `.part` file already holds the whole remote file.
    PartComplete(u64),
    /// `keep_existing` hit an existing final file; settle from disk.
    KeepExisting,
    /// `skip` declined an existing final file; settle as cancelled.
    Skipped(String),
    Cancelled,
    Paused,
}

impl From<EngineError> for RunError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl From<std::io::Error> for RunError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Runs one download to completion; [`DownloadWorker::run`] settles the record.
pub struct DownloadWorker {
    record: DownloadRecord,
    settings: Settings,
    repository: Option<Arc<DownloadRepository>>,
    resolver: Option<Arc<PinningResolver>>,
    events: Option<mpsc::UnboundedSender<EngineEvent>>,
    cancel: CancellationToken,
    pause: Arc<AtomicBool>,
    policy: RetryPolicy,
    client: Option<reqwest::Client>,
    current_url: String,
    final_path: PathBuf,
    part_path: PathBuf,
    /// True when this run continues a non-empty `.part` from an earlier
    /// attempt (a resume); the publish step re-checks the collision policy
    /// because the policy was skipped at prepare time.
    resumed: bool,
    last_emit: Instant,
    /// When this worker started (diagnostics).
    started_at: Instant,
    /// Last HTTP status observed (diagnostics only; never drives control flow).
    diagnostic_status: Option<u16>,
    /// Redirect hops followed so far (diagnostics only).
    redirect_count: u32,
}

impl DownloadWorker {
    /// Create a worker for `record`.
    ///
    /// `record.destination_path` must be a full, already-sanitised path; the
    /// caller owns path-traversal defence. `policy` overrides the settings
    /// derived policy (tests and callers that want explicit delays).
    ///
    /// `resolver` pins this worker's connections to validator-approved
    /// addresses (see [`super::resolver`]); `None` keeps the system resolver.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        record: DownloadRecord,
        settings: Settings,
        repository: Option<Arc<DownloadRepository>>,
        resolver: Option<Arc<PinningResolver>>,
        events: Option<mpsc::UnboundedSender<EngineEvent>>,
        cancel: CancellationToken,
        pause: Arc<AtomicBool>,
        policy: Option<RetryPolicy>,
    ) -> Self {
        let policy = policy.unwrap_or_else(|| policy_from_settings(&settings));
        let final_path = PathBuf::from(&record.destination_path);
        let part_path = with_part_suffix(&final_path);
        let current_url = record.source_url.clone();
        Self {
            record,
            settings,
            repository,
            resolver,
            events,
            cancel,
            pause,
            policy,
            client: None,
            current_url,
            final_path,
            part_path,
            resumed: false,
            last_emit: Instant::now() - PROGRESS_INTERVAL - Duration::from_millis(1),
            started_at: Instant::now(),
            diagnostic_status: None,
            redirect_count: 0,
        }
    }

    /// Execute the full lifecycle and return the settled record.
    ///
    /// Never propagates pause/cancel/download failures: they are reflected on
    /// the record's state and `error_summary`. Collision outcomes are terminal
    /// but not failures (`keep_existing` completes, `skip` cancels).
    pub async fn run(mut self) -> DownloadRecord {
        let outcome = self.run_inner().await;
        match outcome {
            Ok(()) => self.settle_success(),
            Err(RunError::Paused) => self.record.state = DownloadState::Paused,
            Err(RunError::Cancelled) => {
                self.record.state = DownloadState::Cancelled;
                self.record.failure_category =
                    Some(FailureCategory::Cancelled.as_str().to_string());
            }
            Err(RunError::KeepExisting) => self.settle_keep_existing(),
            Err(RunError::Skipped(message)) => {
                // Declining an existing destination settles as cancelled, but
                // the reason is a collision, not a user action.
                self.record.state = DownloadState::Cancelled;
                self.record.error_summary = Some(message);
                self.record.failure_category =
                    Some(FailureCategory::Collision.as_str().to_string());
            }
            Err(RunError::Engine(error)) => {
                let category = error.category();
                self.fail(error.failure_message(), category);
            }
            Err(RunError::Io(error)) => self.fail(
                format!("filesystem error: {error}"),
                FailureCategory::Filesystem,
            ),
            Err(RunError::FreshRestart) | Err(RunError::PartComplete(_)) => self.fail(
                "internal error: unexpected restart signal".to_string(),
                FailureCategory::Unknown,
            ),
        }
        self.persist();
        self.emit_state();
        self.record
    }

    async fn run_inner(&mut self) -> Result<(), RunError> {
        self.prepare_destination().await?;
        let mut resume_offset = self.resume_offset().await?;
        let mut attempt: u32 = 0;

        loop {
            self.check_user_stop(false).await?;
            self.set_state(DownloadState::Connecting);

            let response = match self.open_stream(resume_offset).await {
                Ok(response) => response,
                Err(RunError::PartComplete(total)) => {
                    self.finalize_part(total).await?;
                    return Ok(());
                }
                Err(RunError::FreshRestart) => {
                    resume_offset = 0;
                    continue;
                }
                Err(RunError::Engine(error)) if error.is_transient() => {
                    if !self.retry_or_fail(&error, &mut attempt).await? {
                        return Err(RunError::Engine(error));
                    }
                    continue;
                }
                Err(other) => return Err(other),
            };

            self.set_state(DownloadState::Downloading);
            match self.transfer(response, resume_offset).await {
                Ok(()) => return Ok(()),
                Err(RunError::PartComplete(total)) => {
                    self.finalize_part(total).await?;
                    return Ok(());
                }
                Err(RunError::FreshRestart) => {
                    // Never append across a gap: truncate on the next open.
                    // The retry budget is deliberately *not* reset here, so a
                    // server that keeps dropping mid-body cannot loop forever.
                    resume_offset = 0;
                    continue;
                }
                Err(RunError::Engine(error)) if error.is_transient() => {
                    if !self.retry_or_fail(&error, &mut attempt).await? {
                        return Err(RunError::Engine(error));
                    }
                    // Resume from whatever the `.part` now holds, so a
                    // dropped connection continues instead of re-downloading
                    // the prefix.
                    resume_offset = self.part_size().await;
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Decide, record the attempt, and sleep. Returns `false` when the
    /// policy declines to retry (the caller then fails the download).
    async fn retry_or_fail(
        &mut self,
        error: &EngineError,
        attempt: &mut u32,
    ) -> Result<bool, RunError> {
        let (should, delay) = self.policy.should_retry(&error.outcome(), *attempt);
        if !should {
            return Ok(false);
        }
        tracing::warn!(
            download = ?self.record.id,
            error = %error.sanitized_message(),
            delay = delay,
            attempt = *attempt + 1,
            "transient download error; retrying"
        );
        *attempt += 1;
        self.record.retry_count = *attempt;
        self.persist();
        // Pause-aware: a pause during backoff parks the row as Paused and
        // keeps the `.part`, exactly like a pause mid-transfer.
        self.sleep(delay, true).await?;
        Ok(true)
    }

    // -- destination preparation ----------------------------------------------

    async fn prepare_destination(&mut self) -> Result<(), RunError> {
        let validated = validation::validate_url_for_network_opts(
            &self.record.source_url,
            self.settings.allow_private_addresses,
            true,
        )
        .map_err(EngineError::UnsafeUrl)?;
        self.pin_validated(&validated);
        self.current_url = validated.url;

        if self.record.destination_path.trim().is_empty() {
            return Err(EngineError::Failed("download has no destination path".to_string()).into());
        }
        let final_path = PathBuf::from(&self.record.destination_path);
        if final_path.file_name().is_none() {
            return Err(EngineError::Failed("download has no destination path".to_string()).into());
        }
        if let Some(parent) = final_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        self.part_path = with_part_suffix(&final_path);
        self.final_path = final_path;

        let fresh = !self.part_exists().await || self.part_size().await == 0;
        self.resumed = !fresh;
        if fresh {
            self.apply_collision_policy().await?;
        }
        Ok(())
    }

    /// Honour the collision policy, but only for a fresh (missing/empty) part.
    async fn apply_collision_policy(&mut self) -> Result<(), RunError> {
        let final_path = self.final_path.clone();
        let file_name = final_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        let Some(resolved) =
            file_naming::unique_path(&final_path, self.settings.filename_collision)
        else {
            return Err(RunError::Skipped(format!(
                "destination already exists ({file_name}) and the collision policy is 'skip'; \
                 choose another policy to proceed"
            )));
        };

        if resolved == final_path {
            if resolved.exists() {
                match self.settings.filename_collision {
                    CollisionPolicy::Overwrite => return Ok(()),
                    CollisionPolicy::KeepExisting => return Err(RunError::KeepExisting),
                    CollisionPolicy::Rename | CollisionPolicy::Skip => return Ok(()),
                }
            }
            return Ok(());
        }

        self.record.destination_path = resolved.to_string_lossy().into_owned();
        if let Some(name) = resolved.file_name() {
            self.record.display_name = name.to_string_lossy().into_owned();
        }
        self.part_path = with_part_suffix(&resolved);
        self.final_path = resolved;
        Ok(())
    }

    async fn resume_offset(&mut self) -> Result<u64, RunError> {
        let offset = self.part_size().await;
        if self.record.downloaded_bytes != offset {
            self.record.downloaded_bytes = offset;
            self.persist();
        }
        Ok(offset)
    }

    // -- request construction -------------------------------------------------

    async fn open_stream(&mut self, resume_offset: u64) -> Result<Response, RunError> {
        let client = self.ensure_client()?;
        let mut current = self.current_url.clone();

        for _hop in 0..=MAX_REDIRECT_HOPS {
            self.check_user_stop(false).await?;
            let mut request = client
                .get(&current)
                .header(USER_AGENT, self.settings.user_agent.as_str())
                .header(ACCEPT_ENCODING, "identity");
            if resume_offset > 0 {
                request = request.header(RANGE, format!("bytes={resume_offset}-"));
                if let Some(validator) = self.if_range_validator() {
                    request = request.header(IF_RANGE, validator);
                }
            }

            let mut response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    // reqwest's Display embeds the full URL (signed query and
                    // all), so log only the URL-stripped text.
                    let message = sanitize_transport_error(&error.to_string());
                    tracing::warn!(
                        download = ?self.record.id,
                        url = %validation::redact_url(&current),
                        error = %message,
                        "could not open stream"
                    );
                    return Err(EngineError::Network(error).into());
                }
            };

            let status = response.status().as_u16();
            self.diagnostic_status = Some(status);
            if REDIRECT_STATUSES.contains(&status) {
                let location = header_text(&response, "Location").unwrap_or_default();
                drop(response);
                match validation::validate_redirect(
                    &current,
                    &location,
                    self.settings.allow_private_addresses,
                ) {
                    Ok(validated) => {
                        self.pin_validated(&validated);
                        current = validated.url;
                        self.current_url = current.clone();
                        self.redirect_count += 1;
                        continue;
                    }
                    Err(error) => {
                        return Err(EngineError::Failed(format!(
                            "unsafe redirect target refused: {error}"
                        ))
                        .into());
                    }
                }
            }

            // A bot-verification challenge can arrive with any status (200,
            // 403, 503). Non-2xx bodies are never streamed to disk, so a
            // bounded sniff here is safe and lets the status branches below
            // stay unchanged for genuine auth/not-found/transient failures.
            if !(200..300).contains(&status) {
                let prefix = read_prefix(&mut response).await;
                if is_browser_challenge(&response, &prefix) {
                    drop(response);
                    return Err(challenge_error());
                }
            }

            if status == 401 || status == 403 {
                let html = header_text(&response, "Content-Type")
                    .as_deref()
                    .is_some_and(content_type_is_html);
                drop(response);
                let message = match (status, html) {
                    (401, _) => AUTH_REQUIRED_MESSAGE,
                    (_, true) => AUTH_BLOCKED_HTML_MESSAGE,
                    (_, false) => AUTH_BLOCKED_MESSAGE,
                };
                return Err(EngineError::AuthRequired { message }.into());
            }
            if status == 404 || status == 410 {
                // A file locker whose link expired/vanished often answers with
                // its HTML landing page; surface that so the user knows a
                // browser may be required instead of chasing a network issue.
                let html = header_text(&response, "Content-Type")
                    .as_deref()
                    .is_some_and(content_type_is_html);
                drop(response);
                let hint = if html { NOT_FOUND_HTML_HINT } else { "" };
                return Err(EngineError::NotFound { status, hint }.into());
            }
            if status == 416 && resume_offset > 0 {
                let total = total_from_416(&response);
                drop(response);
                if let Some(total) = total
                    && resume_offset == total
                {
                    return Err(RunError::PartComplete(total));
                }
                return Err(RunError::FreshRestart);
            }
            if TRANSIENT_STATUS_CODES.contains(&status) {
                let retry_after = header_text(&response, "Retry-After")
                    .and_then(|text| parse_retry_after(Some(&text), SystemTime::now()));
                drop(response);
                return Err(EngineError::TransientHttp {
                    status,
                    retry_after,
                }
                .into());
            }
            if !(200..300).contains(&status) {
                drop(response);
                return Err(EngineError::UnexpectedStatus(status).into());
            }
            return Ok(response);
        }

        Err(EngineError::Failed(format!(
            "too many redirects (more than {MAX_REDIRECT_HOPS}); \
             the source may have a redirect loop"
        ))
        .into())
    }

    fn ensure_client(&mut self) -> Result<reqwest::Client, RunError> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let resolver: Option<Arc<dyn reqwest::dns::Resolve>> = self
            .resolver
            .as_ref()
            .map(|resolver| Arc::clone(resolver) as Arc<dyn reqwest::dns::Resolve>);
        let client = build_client_with_resolver(&self.settings, resolver).map_err(|error| {
            EngineError::Failed(format!("could not build HTTP client: {error}"))
        })?;
        self.client = Some(client.clone());
        Ok(client)
    }

    /// Pin the host to the addresses the validator approved.
    ///
    /// Called for the initial URL and after every redirect hop, so the
    /// connection can only dial an address this process screened itself.
    fn pin_validated(&self, validated: &validation::ValidatedUrl) {
        if let Some(resolver) = &self.resolver {
            resolver.pin(&validated.host, &validated.resolved_ips);
        }
    }

    fn if_range_validator(&self) -> Option<&str> {
        select_if_range_validator(
            self.record.etag.as_deref(),
            self.record.last_modified.as_deref(),
        )
    }

    // -- transfer -------------------------------------------------------------

    async fn transfer(
        &mut self,
        mut response: Response,
        resume_offset: u64,
    ) -> Result<(), RunError> {
        let status = response.status().as_u16();
        let content_type = header_text(&response, "Content-Type")
            .unwrap_or_default()
            .trim()
            .to_string();
        let header_length = header_total(&response);
        let is_resume = status == 206;

        if is_resume {
            let raw = header_text(&response, "Content-Range").unwrap_or_default();
            if content_range_start(&raw) != Some(resume_offset) {
                tracing::warn!(
                    download = ?self.record.id,
                    content_range = %raw,
                    "unusable Content-Range for resume; restarting from zero"
                );
                return Err(RunError::FreshRestart);
            }
            if let Some(total) = content_range_total(&raw)
                && total > 0
            {
                self.record.expected_size = Some(total);
                if resume_offset == total {
                    return Err(RunError::PartComplete(total));
                }
            }
        } else if resume_offset > 0 {
            tracing::info!(
                download = ?self.record.id,
                "server ignored Range; restarting from zero"
            );
            return Err(RunError::FreshRestart);
        }

        let accept_ranges = is_resume
            || header_text(&response, "Accept-Ranges")
                .map(|value| value.to_ascii_lowercase().contains("bytes"))
                .unwrap_or(false);

        // Capture validators and the content type *before* the HTML check so
        // a landing/login page still leaves diagnostic context on the row.
        self.capture_validators(&response, &content_type);

        // Refuse an HTML landing/login page before writing any bytes.
        if content_type_is_html(&content_type) {
            let prefix = read_prefix(&mut response).await;
            if is_browser_challenge(&response, &prefix) {
                return Err(challenge_error());
            }
            return Err(html_error());
        }

        let progress_total = if is_resume {
            self.record.expected_size
        } else {
            header_length
        };
        if !is_resume && let Some(length) = header_length {
            self.record.expected_size = Some(length);
        }
        let verify_length = header_length;

        let append = is_resume && resume_offset > 0;
        let mut file = if append {
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.part_path)
                .await?
        } else {
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.part_path)
                .await?
        };

        let mut digest = Sha256::new();
        let mut received: u64 = 0;
        // A 200 body is sniffed for HTML using a bounded rolling prefix. The
        // 206 resume path is exempt: mid-file bytes are legitimately binary.
        let mut sniff = Vec::new();
        let mut sniffing = resume_offset == 0;
        let started = Instant::now();

        loop {
            self.check_user_stop(accept_ranges).await?;

            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    // A body truncated mid-transfer (even with a
                    // Content-Length) is a transient transport failure: the
                    // retry resumes from whatever the `.part` already holds.
                    return Err(EngineError::Network(error).into());
                }
            };

            if sniffing {
                let remaining = HTML_SNIFF_LIMIT.saturating_sub(sniff.len());
                let take = chunk.len().min(remaining);
                sniff.extend_from_slice(&chunk[..take]);
                let verdict = match sniff_html_prefix(&sniff) {
                    PrefixSniff::NeedMore if sniff.len() < HTML_SNIFF_LIMIT => {
                        // Hold the bytes until the prefix can be classified.
                        continue;
                    }
                    PrefixSniff::NeedMore => {
                        // A full 8 KiB of leading whitespace is not HTML;
                        // treat the body as binary instead of buffering on.
                        PrefixSniff::NotHtml
                    }
                    decided => decided,
                };
                if verdict == PrefixSniff::Html {
                    if is_browser_challenge(&response, &sniff) {
                        return Err(challenge_error());
                    }
                    return Err(html_error());
                }
                sniffing = false;
                // Flush the held prefix and the rest of this chunk in order.
                let mut pending = std::mem::take(&mut sniff);
                pending.extend_from_slice(&chunk[take..]);
                file.write_all(&pending).await?;
                digest.update(&pending);
                received += pending.len() as u64;
                self.maybe_throttle(started, received, accept_ranges)
                    .await?;
                self.maybe_emit_progress(
                    progress_total,
                    resume_offset + received,
                    received,
                    started,
                );
                continue;
            }

            file.write_all(&chunk).await?;
            digest.update(&chunk);
            received += chunk.len() as u64;
            self.maybe_throttle(started, received, accept_ranges)
                .await?;
            self.maybe_emit_progress(progress_total, resume_offset + received, received, started);
        }

        // The body ended before the prefix could be classified (a tiny binary
        // file starting with something like `<h`): decide on what we have.
        if sniffing && !sniff.is_empty() {
            if looks_like_html(&sniff) {
                if is_browser_challenge(&response, &sniff) {
                    return Err(challenge_error());
                }
                return Err(html_error());
            }
            file.write_all(&sniff).await?;
            digest.update(&sniff);
            received += sniff.len() as u64;
            self.maybe_emit_progress(progress_total, resume_offset + received, received, started);
        }

        file.flush().await?;
        // Best effort: make the bytes durable before the rename publishes
        // them. A failed fsync must not fail an otherwise good download.
        if let Err(error) = file.sync_all().await {
            tracing::warn!(
                download = ?self.record.id,
                error = %error,
                "could not fsync .part before publishing"
            );
        }
        drop(file);

        if let Some(expected) = verify_length
            && received != expected
        {
            self.discard_part("Content-Length mismatch").await;
            return Err(EngineError::Failed(format!(
                "server sent {received} bytes but promised {expected} (Content-Length mismatch)"
            ))
            .into());
        }

        let total_size = resume_offset + received;
        let stream_digest = hex_digest(&digest.finalize());
        if let Some(expected) = normalize_checksum(self.record.checksum_sha256.as_deref()) {
            // Resume: hash the whole file, not just this response's bytes.
            let actual = if resume_offset == 0 {
                stream_digest
            } else {
                hash_file(&self.part_path).await?
            };
            if actual != expected {
                self.discard_part("SHA-256 mismatch").await;
                return Err(checksum_mismatch(&expected, &actual));
            }
            self.record.checksum_verified = true;
        }

        self.publish(total_size).await?;
        let speed = instance_speed(total_size, started);
        self.emit_progress(progress_total, total_size, speed, Some(0));
        Ok(())
    }

    fn capture_validators(&mut self, response: &Response, content_type: &str) {
        if let Some(etag) = header_text(response, "ETag").filter(|value| !value.is_empty()) {
            self.record.etag = Some(etag);
        }
        if let Some(last_modified) =
            header_text(response, "Last-Modified").filter(|value| !value.is_empty())
        {
            self.record.last_modified = Some(last_modified);
        }
        self.record.content_type = if content_type.is_empty() {
            None
        } else {
            Some(content_type.to_string())
        };
    }

    /// Publish a `.part` that already holds the entire remote file.
    async fn finalize_part(&mut self, total: u64) -> Result<(), RunError> {
        let actual = self.part_size().await;
        if actual != total {
            self.discard_part("resume size mismatch").await;
            return Err(EngineError::Failed(format!(
                "resume covers the whole file but the .part holds {actual} bytes \
                 (server reported {total}); restarting download"
            ))
            .into());
        }
        if let Some(expected) = normalize_checksum(self.record.checksum_sha256.as_deref()) {
            let actual_hash = hash_file(&self.part_path).await?;
            if actual_hash != expected {
                self.discard_part("SHA-256 mismatch").await;
                return Err(checksum_mismatch(&expected, &actual_hash));
            }
            self.record.checksum_verified = true;
        }
        self.publish(total).await
    }

    /// Atomically move `.part` over the final name, then verify the size.
    async fn publish(&mut self, total: u64) -> Result<(), RunError> {
        let part_size = self.part_size().await;
        if part_size != total {
            self.discard_part("final size mismatch").await;
            return Err(EngineError::Failed(format!(
                "final file size {part_size} != downloaded {total} bytes (filesystem error)"
            ))
            .into());
        }
        if let Some(parent) = self.final_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        // A resumed `.part` skipped the collision policy at prepare time. The
        // final file may have appeared since (another download finished), so
        // honour skip/keep_existing/rename here instead of renaming over it.
        if self.resumed && self.final_path.exists() {
            match self.settings.filename_collision {
                CollisionPolicy::Overwrite => {}
                CollisionPolicy::Skip => {
                    self.discard_part("collision policy 'skip' on resume").await;
                    let file_name = self
                        .final_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    return Err(RunError::Skipped(format!(
                        "destination already exists ({file_name}) and the collision policy is \
                         'skip'; choose another policy to proceed"
                    )));
                }
                CollisionPolicy::KeepExisting => {
                    self.discard_part("collision policy 'keep_existing' on resume")
                        .await;
                    return Err(RunError::KeepExisting);
                }
                CollisionPolicy::Rename => {
                    let Some(resolved) =
                        file_naming::unique_path(&self.final_path, CollisionPolicy::Rename)
                    else {
                        return Err(EngineError::Failed(
                            "could not find a free filename for the resumed download".to_string(),
                        )
                        .into());
                    };
                    self.record.destination_path = resolved.to_string_lossy().into_owned();
                    if let Some(name) = resolved.file_name() {
                        self.record.display_name = name.to_string_lossy().into_owned();
                    }
                    self.final_path = resolved;
                }
            }
        }

        match tokio::fs::rename(&self.part_path, &self.final_path).await {
            Ok(()) => {}
            // Windows refuses to replace an existing destination; the
            // overwrite policy is the only path that can reach this. Only
            // remove the destination when the rename *actually* failed for
            // that reason: any other error must propagate so a good file is
            // never deleted.
            Err(error) if rename_blocked_by_existing(&error) && self.final_path.exists() => {
                tokio::fs::remove_file(&self.final_path).await?;
                tokio::fs::rename(&self.part_path, &self.final_path).await?;
            }
            Err(error) => return Err(RunError::Io(error)),
        }

        let actual = tokio::fs::metadata(&self.final_path).await?.len();
        if actual != total {
            return Err(EngineError::Failed(format!(
                "final file size {actual} != downloaded {total} bytes (filesystem error)"
            ))
            .into());
        }
        self.record.downloaded_bytes = total;
        Ok(())
    }

    // -- cooperative stop, throttling, progress ------------------------------

    /// `_check_user_stop` port.
    ///
    /// Cancel always wins. Pause keeps the `.part` only when the server can
    /// resume (206 or `Accept-Ranges: bytes`); otherwise pausing behaves like
    /// cancelling, exactly as in the legacy worker.
    async fn check_user_stop(&mut self, accept_ranges: bool) -> Result<(), RunError> {
        if self.cancel.is_cancelled() {
            self.persist();
            return Err(RunError::Cancelled);
        }
        if self.pause.load(Ordering::SeqCst) {
            if accept_ranges {
                self.record.downloaded_bytes = self.part_size().await;
                self.persist();
                return Err(RunError::Paused);
            }
            self.persist();
            return Err(RunError::Cancelled);
        }
        Ok(())
    }

    /// Cancel/pause-aware sleep in at most 100 ms slices.
    ///
    /// The ceiling matches [`MAX_RETRY_AFTER_SECONDS`], so even if a server
    /// hint somehow bypassed the policy clamp the worker cannot park for
    /// longer than 15 minutes.
    async fn sleep(&mut self, seconds: f64, accept_ranges: bool) -> Result<(), RunError> {
        let seconds = if seconds.is_finite() {
            seconds.clamp(0.0, MAX_RETRY_AFTER_SECONDS)
        } else {
            0.0
        };
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < deadline {
            self.check_user_stop(accept_ranges).await?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        }
        Ok(())
    }

    async fn maybe_throttle(
        &mut self,
        started: Instant,
        received: u64,
        accept_ranges: bool,
    ) -> Result<(), RunError> {
        let limit = self.settings.bandwidth_limit_bps;
        if limit == 0 {
            return Ok(());
        }
        let elapsed = started.elapsed().as_secs_f64();
        let target = received as f64 / limit as f64;
        if target > elapsed {
            self.sleep((target - elapsed).min(5.0), accept_ranges)
                .await?;
        }
        Ok(())
    }

    fn maybe_emit_progress(
        &mut self,
        total: Option<u64>,
        done: u64,
        speed_done: u64,
        started: Instant,
    ) {
        let now = Instant::now();
        if now.duration_since(self.last_emit) >= PROGRESS_INTERVAL {
            self.last_emit = now;
            let speed = instance_speed(speed_done, started);
            let eta = compute_eta(total, done, speed);
            self.emit_progress(total, done, speed, eta);
        }
    }

    fn emit_progress(
        &self,
        total: Option<u64>,
        done: u64,
        speed_bps: f64,
        eta_seconds: Option<u64>,
    ) {
        let Some(events) = &self.events else {
            return;
        };
        let snapshot = DownloadProgress {
            download_id: self.record.id.unwrap_or(-1),
            downloaded_bytes: done,
            total_bytes: total,
            speed_bps,
            eta_seconds,
        };
        if events.send(EngineEvent::Progress(snapshot)).is_err() {
            tracing::debug!("progress observer channel closed");
        }
    }

    // -- state + persistence --------------------------------------------------

    fn set_state(&mut self, state: DownloadState) {
        self.record.state = state;
        self.persist();
        self.emit_state();
    }

    fn settle_success(&mut self) {
        self.record.state = DownloadState::Completed;
        self.record.completed_at = Some(now_iso());
        // Deliberate improvement over legacy: a completed download must not
        // keep a stale failure summary from an earlier attempt.
        self.record.error_summary = None;
        self.record.failure_category = None;
    }

    fn settle_keep_existing(&mut self) {
        let path = PathBuf::from(&self.record.destination_path);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                self.record.downloaded_bytes = metadata.len();
                self.record.state = DownloadState::Completed;
                self.record.completed_at = Some(now_iso());
                self.record.error_summary = None;
                tracing::info!(
                    download = ?self.record.id,
                    "existing file kept (keep_existing); marked completed"
                );
            }
            _ => self.fail(
                "collision policy is 'keep_existing' but the existing file is no longer \
                 present on disk; retry to download it again"
                    .to_string(),
                FailureCategory::Collision,
            ),
        }
    }

    /// Settle the record as failed and emit a redacted diagnostic.
    ///
    /// `message` is the user-facing `error_summary` (unchanged wording); the
    /// category and the structured context are stored/logged separately.
    fn fail(&mut self, message: String, category: FailureCategory) {
        self.record.state = DownloadState::Failed;
        self.record.error_summary = Some(message);
        self.record.failure_category = Some(category.as_str().to_string());

        let diagnostic = Diagnostic {
            download_id: self.record.id,
            host: host_from_url(&self.current_url),
            redirect_count: self.redirect_count,
            status: self.diagnostic_status,
            content_type: self.record.content_type.clone(),
            category,
            retries: self.record.retry_count,
            elapsed_seconds: self.started_at.elapsed().as_secs_f64(),
        };
        // Host only: never the URL, its query string, or any credentials.
        tracing::warn!(
            download = ?diagnostic.download_id,
            category = diagnostic.category.as_str(),
            host = %diagnostic.host,
            status = ?diagnostic.status,
            redirects = diagnostic.redirect_count,
            retries = diagnostic.retries,
            elapsed_seconds = diagnostic.elapsed_seconds,
            "download failed"
        );
    }

    fn persist(&self) {
        if let Some(repository) = &self.repository
            && self.record.id.is_some()
            && let Err(error) = repository.update(&self.record)
        {
            tracing::warn!(
                download = ?self.record.id,
                error = %error,
                "could not persist download state"
            );
        }
    }

    fn emit_state(&self) {
        if let Some(events) = &self.events {
            let _ = events.send(EngineEvent::StateChanged(Box::new(self.record.clone())));
        }
    }

    async fn part_size(&self) -> u64 {
        match tokio::fs::metadata(&self.part_path).await {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            _ => 0,
        }
    }

    async fn part_exists(&self) -> bool {
        tokio::fs::metadata(&self.part_path)
            .await
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    }

    /// Delete a `.part` whose bytes can no longer be trusted.
    ///
    /// Used by permanent post-write failures (checksum and size mismatches)
    /// so a manual retry starts from zero instead of resuming poisoned bytes
    /// and failing the same way forever. Best effort: a missing file is fine
    /// and a failed removal is only logged, never fatal.
    async fn discard_part(&self, reason: &str) {
        match tokio::fs::remove_file(&self.part_path).await {
            Ok(()) => tracing::warn!(
                download = ?self.record.id,
                reason = reason,
                "deleted stale .part after a permanent failure"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                download = ?self.record.id,
                error = %error,
                "could not delete stale .part"
            ),
        }
    }
}

// -- free helpers -------------------------------------------------------------

fn policy_from_settings(settings: &Settings) -> RetryPolicy {
    let base = settings.retry_delay.clamp(0.0, MAX_DELAY_SECONDS);
    let max = settings.retry_delay.max(MAX_DELAY_SECONDS).max(base);
    RetryPolicy::new(settings.retry_count, base, max).unwrap_or(RetryPolicy {
        max_retries: settings.retry_count,
        base_delay: base,
        max_delay: max,
    })
}

fn with_part_suffix(path: &Path) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(".part");
    PathBuf::from(raw)
}

/// True when a rename failed only because the destination already exists.
///
/// Windows reports this through `AlreadyExists` or the raw codes
/// `ERROR_FILE_EXISTS` (80) / `ERROR_ALREADY_EXISTS` (183). Any other error
/// (permission denied, sharing violation, disk failure) must not trigger the
/// destructive remove-and-retry path.
fn rename_blocked_by_existing(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(80) | Some(183))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Pick the `If-Range` validator, preferring a strong `ETag`.
///
/// RFC 9110 forbids a weak validator (`W/"..."`) in `If-Range`, so a weak
/// ETag is skipped in favour of `Last-Modified` (always strong).
fn select_if_range_validator<'a>(
    etag: Option<&'a str>,
    last_modified: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(etag) = etag
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.starts_with("W/"))
    {
        return Some(etag);
    }
    last_modified
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Strip the request URL that `reqwest::Error`'s `Display` embeds.
///
/// reqwest renders `... for url (<full url>)`, and a signed query string in
/// that URL must never reach a log line or the persisted `error_summary`.
/// Everything from the marker to the end of the message is dropped.
fn sanitize_transport_error(text: &str) -> String {
    match text.find(" for url (") {
        Some(index) => text[..index].trim_end().to_string(),
        None => text.to_string(),
    }
}

fn header_text(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn header_total(response: &Response) -> Option<u64> {
    header_text(response, "Content-Length")?
        .trim()
        .parse::<u64>()
        .ok()
}

fn content_range_parts(raw: &str) -> Option<(&str, &str)> {
    let text = raw.trim();
    if !text.to_ascii_lowercase().starts_with("bytes ") {
        return None;
    }
    let body = text.get(6..)?;
    let (span, total) = body.split_once('/')?;
    Some((span.trim(), total.trim()))
}

fn content_range_start(raw: &str) -> Option<u64> {
    let (span, _) = content_range_parts(raw)?;
    span.split('-').next()?.trim().parse::<u64>().ok()
}

fn content_range_total(raw: &str) -> Option<u64> {
    let (_, total) = content_range_parts(raw)?;
    if total == "*" {
        None
    } else {
        total.parse::<u64>().ok()
    }
}

fn total_from_416(response: &Response) -> Option<u64> {
    let raw = header_text(response, "Content-Range").unwrap_or_default();
    content_range_total(&raw).filter(|total| *total > 0)
}

fn content_type_is_html(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/html")
}

/// Upper bound on the response prefix inspected for HTML markers.
const HTML_SNIFF_LIMIT: usize = CHALLENGE_SNIFF_LIMIT;

/// Leading byte sequences that identify an HTML/XML document.
const HTML_START_MARKERS: [&[u8]; 7] = [
    b"<!doctype",
    b"<html",
    b"<head",
    b"<body",
    b"<script",
    b"<!--",
    b"<?xml",
];

/// Result of inspecting a response prefix for HTML markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixSniff {
    /// The prefix starts an HTML/XML document.
    Html,
    /// The prefix cannot be HTML.
    NotHtml,
    /// More bytes are needed before the decision is possible.
    NeedMore,
}

fn is_sniff_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0B | 0x0C)
}

/// Body prefix with a UTF-8 BOM and leading whitespace removed.
///
/// Returns `None` while the prefix could still be a partial BOM, because the
/// bytes following it are not yet known.
fn html_head(bytes: &[u8]) -> Option<&[u8]> {
    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
    let rest = if bytes.starts_with(&BOM) {
        &bytes[BOM.len()..]
    } else if bytes.len() < BOM.len() && BOM.starts_with(bytes) {
        return None;
    } else {
        bytes
    };
    let start = rest
        .iter()
        .position(|byte| !is_sniff_whitespace(*byte))
        .unwrap_or(rest.len());
    Some(&rest[start..])
}

/// Classify a response prefix as HTML, definitely-not-HTML, or undecidable.
fn sniff_html_prefix(bytes: &[u8]) -> PrefixSniff {
    let Some(head) = html_head(bytes) else {
        return PrefixSniff::NeedMore;
    };
    if head.is_empty() {
        return PrefixSniff::NeedMore;
    }
    let probe = &head[..head.len().min(16)];
    let lowered = probe.to_ascii_lowercase();
    if HTML_START_MARKERS
        .iter()
        .any(|marker| lowered.starts_with(marker))
    {
        return PrefixSniff::Html;
    }
    if HTML_START_MARKERS
        .iter()
        .any(|marker| marker.starts_with(&lowered))
    {
        return PrefixSniff::NeedMore;
    }
    PrefixSniff::NotHtml
}

fn looks_like_html(bytes: &[u8]) -> bool {
    matches!(sniff_html_prefix(bytes), PrefixSniff::Html)
}

/// Read at most [`CHALLENGE_SNIFF_LIMIT`] bytes from a response that is about
/// to be rejected (a non-2xx response, or one already known to be HTML).
///
/// The cap keeps a hostile or endless error body from being buffered without
/// bound; the bytes are only used to look for challenge-page markers.
async fn read_prefix(response: &mut Response) -> Vec<u8> {
    let mut prefix = Vec::new();
    while prefix.len() < CHALLENGE_SNIFF_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = CHALLENGE_SNIFF_LIMIT - prefix.len();
                let take = chunk.len().min(remaining);
                prefix.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    prefix
}

/// True when the response looks like an interactive browser-verification
/// challenge (Cloudflare managed challenge, DDoS-Guard, ...).
///
/// Any one of these signals is sufficient:
/// - `cf-mitigated: challenge|captcha|block`
/// - a body marker specific to such pages
/// - `server: cloudflare` plus the generic "Just a moment" marker
fn is_browser_challenge(response: &Response, body_prefix: &[u8]) -> bool {
    if header_text(response, "cf-mitigated").is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "challenge" | "captcha" | "block"
        )
    }) {
        return true;
    }

    let head = &body_prefix[..body_prefix.len().min(CHALLENGE_SNIFF_LIMIT)];
    if head.is_empty() {
        return false;
    }
    let lowered = String::from_utf8_lossy(head).to_ascii_lowercase();
    if CHALLENGE_BODY_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return true;
    }
    has_cloudflare_edge(response) && lowered.contains("just a moment")
}

fn has_cloudflare_edge(response: &Response) -> bool {
    header_text(response, "Server")
        .is_some_and(|value| value.to_ascii_lowercase().contains("cloudflare"))
}

fn challenge_error() -> RunError {
    EngineError::AuthRequired {
        message: BROWSER_CHALLENGE_MESSAGE,
    }
    .into()
}

fn normalize_checksum(value: Option<&str>) -> Option<String> {
    let lowered = value?.trim().to_ascii_lowercase();
    let hex = lowered
        .strip_prefix("sha256:")
        .map(str::trim)
        .unwrap_or(lowered.as_str());
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(hex.to_string())
    } else {
        None
    }
}

async fn hash_file(path: &Path) -> Result<String, RunError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn instance_speed(done: u64, started: Instant) -> f64 {
    let elapsed = started.elapsed().as_secs_f64();
    if elapsed > 0.0 {
        done as f64 / elapsed
    } else {
        0.0
    }
}

fn compute_eta(total: Option<u64>, done: u64, speed_bps: f64) -> Option<u64> {
    let total = total?;
    if total <= done {
        return Some(0);
    }
    if speed_bps <= 0.0 {
        return None;
    }
    let remaining = (total - done) as f64;
    Some((remaining / speed_bps) as u64)
}

/// True when a safety error names a private/loopback destination.
///
/// Covers both wording families: the validation layer ("refusing to connect
/// to private/loopback address", "all resolved addresses ...") and the
/// connection-time resolver ("... after private-address screening").
fn is_private_address_message(message: &str) -> bool {
    message.contains("private-address screening")
        || message.contains("refusing to connect to private/loopback address")
        || message.contains("all resolved addresses")
}

/// Classify an HTTP status into a diagnostics category.
fn classify_status(status: u16) -> FailureCategory {
    match status {
        401 | 403 => FailureCategory::AuthRequired,
        404 | 410 => FailureCategory::NotFound,
        408 => FailureCategory::Timeout,
        429 | 500..=599 => FailureCategory::ServerError,
        _ => FailureCategory::Unknown,
    }
}

/// Classify a message the engine itself produced.
///
/// The `error_summary` strings are frozen (tests and stored rows depend on
/// their exact wording), so this maps the prefixes the worker generates rather
/// than rewording them. Anything unrecognised is `Unknown` — never a panic.
fn classify_failure_message(message: &str) -> FailureCategory {
    if message == HTML_ERROR {
        return FailureCategory::HtmlPage;
    }
    if is_private_address_message(message) {
        return FailureCategory::PrivateAddress;
    }
    if message.starts_with("unsafe redirect target refused")
        || message.starts_with("too many redirects")
    {
        return FailureCategory::RedirectRejected;
    }
    if message.starts_with("SHA-256 checksum mismatch") {
        return FailureCategory::ChecksumMismatch;
    }
    if message.starts_with("filesystem error") {
        return FailureCategory::Filesystem;
    }
    if message.contains("destination already exists")
        || message.starts_with("collision policy is 'keep_existing'")
    {
        return FailureCategory::Collision;
    }
    if message.contains("size mismatch") || message.contains("Content-Length mismatch") {
        return FailureCategory::Network;
    }
    FailureCategory::Unknown
}

fn html_error() -> RunError {
    EngineError::Failed(HTML_ERROR.to_string()).into()
}

fn checksum_mismatch(expected: &str, actual: &str) -> RunError {
    EngineError::Failed(format!(
        "SHA-256 checksum mismatch: expected {expected}, got {actual}; \
         the file is corrupt or the server delivered different content"
    ))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_errors_lose_their_embedded_url() {
        let text = "error sending request for url (https://cdn.example/f.zip?token=secret)";
        assert_eq!(sanitize_transport_error(text), "error sending request");
        assert_eq!(
            sanitize_transport_error("connection reset by peer"),
            "connection reset by peer"
        );
    }

    #[test]
    fn weak_etags_are_not_used_for_if_range() {
        assert_eq!(select_if_range_validator(Some("W/\"weak\""), None), None);
        assert_eq!(
            select_if_range_validator(Some("W/\"weak\""), Some("Mon, 01 Jan 2024 00:00:00 GMT")),
            Some("Mon, 01 Jan 2024 00:00:00 GMT")
        );
        assert_eq!(
            select_if_range_validator(Some(" \"strong\" "), None),
            Some("\"strong\"")
        );
        assert_eq!(
            select_if_range_validator(Some("   "), Some("date")),
            Some("date")
        );
    }

    #[test]
    fn rename_conflicts_are_classified_narrowly() {
        assert!(rename_blocked_by_existing(&std::io::Error::from(
            std::io::ErrorKind::AlreadyExists
        )));
        assert!(!rename_blocked_by_existing(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        #[cfg(windows)]
        {
            assert!(rename_blocked_by_existing(
                &std::io::Error::from_raw_os_error(80)
            ));
            assert!(rename_blocked_by_existing(
                &std::io::Error::from_raw_os_error(183)
            ));
            assert!(!rename_blocked_by_existing(
                &std::io::Error::from_raw_os_error(32)
            ));
        }
    }

    #[test]
    fn html_sniff_waits_for_enough_prefix() {
        assert_eq!(sniff_html_prefix(b""), PrefixSniff::NeedMore);
        assert_eq!(sniff_html_prefix(b"<h"), PrefixSniff::NeedMore);
        assert_eq!(sniff_html_prefix(b"<html>"), PrefixSniff::Html);
        assert_eq!(
            sniff_html_prefix(b"\xEF\xBB\xBF  <!DOCTYPE html>"),
            PrefixSniff::Html
        );
        assert_eq!(sniff_html_prefix(b"<!-- x -->\n<html>"), PrefixSniff::Html);
        assert_eq!(
            sniff_html_prefix(b"<?xml version=\"1.0\"?>"),
            PrefixSniff::Html
        );
        assert_eq!(sniff_html_prefix(b"<body>"), PrefixSniff::Html);
        assert_eq!(sniff_html_prefix(b"<head>"), PrefixSniff::Html);
        assert_eq!(sniff_html_prefix(b"<script>"), PrefixSniff::Html);
        assert_eq!(sniff_html_prefix(b"PK\x03\x04"), PrefixSniff::NotHtml);
        assert_eq!(sniff_html_prefix(b"\x89PNG"), PrefixSniff::NotHtml);
        assert!(looks_like_html(b"<html>"));
        assert!(!looks_like_html(b"<h"));
    }
}

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
use crate::models::download_item::{DownloadRecord, now_iso};
use crate::models::download_status::DownloadState;
use crate::services::{file_naming, validation};

use super::client::build_client_with_resolver;
use super::progress::DownloadProgress;
use super::resolver::PinningResolver;
use super::retry::{RetryOutcome, RetryPolicy, TRANSIENT_STATUS_CODES, parse_retry_after};

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

    /// 401/403: retrying will not help.
    #[error(
        "server requires authentication (HTTP {0}); configure credentials/cookies for this source"
    )]
    AuthRequired(u16),

    /// 404/410: the file is gone.
    #[error("file not found on server (HTTP {0})")]
    NotFound(u16),

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
            Self::TransientHttp { .. } | Self::Network(_) => format!("network failure: {self}"),
            _ => self.to_string(),
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
    last_emit: Instant,
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
            last_emit: Instant::now() - PROGRESS_INTERVAL - Duration::from_millis(1),
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
            Err(RunError::Cancelled) => self.record.state = DownloadState::Cancelled,
            Err(RunError::KeepExisting) => self.settle_keep_existing(),
            Err(RunError::Skipped(message)) => {
                self.record.state = DownloadState::Cancelled;
                self.record.error_summary = Some(message);
            }
            Err(RunError::Engine(error)) => self.fail(error.failure_message()),
            Err(RunError::Io(error)) => self.fail(format!("filesystem error: {error}")),
            Err(RunError::FreshRestart) | Err(RunError::PartComplete(_)) => {
                self.fail("internal error: unexpected restart signal".to_string());
            }
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
                    resume_offset = 0;
                    attempt = 0;
                    continue;
                }
                Err(RunError::Engine(error)) if error.is_transient() => {
                    if !self.retry_or_fail(&error, &mut attempt).await? {
                        return Err(RunError::Engine(error));
                    }
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
            error = %error,
            delay = delay,
            attempt = *attempt + 1,
            "transient download error; retrying"
        );
        *attempt += 1;
        self.record.retry_count = *attempt;
        self.persist();
        self.sleep(delay, false).await?;
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

            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(
                        download = ?self.record.id,
                        url = %validation::redact_url(&current),
                        error = %error,
                        "could not open stream"
                    );
                    return Err(EngineError::Network(error).into());
                }
            };

            let status = response.status().as_u16();
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

            if status == 401 || status == 403 {
                drop(response);
                return Err(EngineError::AuthRequired(status).into());
            }
            if status == 404 || status == 410 {
                drop(response);
                return Err(EngineError::NotFound(status).into());
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
        self.record
            .etag
            .as_deref()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.record
                    .last_modified
                    .as_deref()
                    .filter(|value| !value.is_empty())
            })
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

        // Refuse an HTML landing/login page before writing any bytes.
        if content_type_is_html(&content_type) {
            return Err(html_error());
        }

        self.capture_validators(&response, &content_type);
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
        let mut got_first_chunk = false;
        let started = Instant::now();

        loop {
            self.check_user_stop(accept_ranges).await?;

            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    if let Some(expected) = verify_length
                        && received < expected
                    {
                        return Err(EngineError::Failed(format!(
                            "size mismatch: the server closed the connection before the complete \
                             message body ({received} bytes received)"
                        ))
                        .into());
                    }
                    return Err(EngineError::Network(error).into());
                }
            };

            if !got_first_chunk {
                got_first_chunk = true;
                if resume_offset == 0 && looks_like_html(&chunk) {
                    return Err(html_error());
                }
            }

            file.write_all(&chunk).await?;
            digest.update(&chunk);
            received += chunk.len() as u64;
            self.maybe_throttle(started, received, accept_ranges)
                .await?;
            self.maybe_emit_progress(progress_total, resume_offset + received, received, started);
        }

        file.flush().await?;
        drop(file);

        if let Some(expected) = verify_length
            && received != expected
        {
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
            return Err(EngineError::Failed(format!(
                "resume covers the whole file but the .part holds {actual} bytes \
                 (server reported {total}); restarting download"
            ))
            .into());
        }
        if let Some(expected) = normalize_checksum(self.record.checksum_sha256.as_deref()) {
            let actual_hash = hash_file(&self.part_path).await?;
            if actual_hash != expected {
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

        match tokio::fs::rename(&self.part_path, &self.final_path).await {
            Ok(()) => {}
            // Windows refuses to replace an existing destination; the
            // overwrite policy is the only path that can reach this.
            Err(_) if self.final_path.exists() => {
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
    async fn sleep(&mut self, seconds: f64, accept_ranges: bool) -> Result<(), RunError> {
        let seconds = if seconds.is_finite() {
            seconds.clamp(0.0, 86_400.0)
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
            ),
        }
    }

    fn fail(&mut self, message: String) {
        self.record.state = DownloadState::Failed;
        self.record.error_summary = Some(message);
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

fn looks_like_html(chunk: &[u8]) -> bool {
    let mut head = chunk;
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        head = &head[3..];
    }
    while let [first, rest @ ..] = head {
        if matches!(*first, b' ' | b'\t' | b'\r' | b'\n' | 0x0B | 0x0C) {
            head = rest;
        } else {
            break;
        }
    }
    let head = &head[..head.len().min(1024)];
    let lowered = head.to_ascii_lowercase();
    lowered.starts_with(b"<!doctype html") || lowered.starts_with(b"<html")
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

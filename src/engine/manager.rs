//! Queue scheduling and high-level download control.
//!
//! Port of the legacy Python ``src.core.queue_scheduler`` and
//! ``src.core.download_manager``:
//!
//! - [`QueueScheduler`] enforces the ``max_simultaneous`` cap atomically,
//! - [`DownloadManager`] adds downloads, starts queued work, and exposes
//!   pause / resume / retry / cancel / remove controls,
//! - every spawned worker reports through one shared event channel that the
//!   UI consumes via [`DownloadManager::subscribe`].
//!
//! Scheduling is deterministic: queued rows are started oldest-id first, and
//! releasing a slot immediately pumps the next queued row.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Settings;
use crate::database::repository::DownloadRepository;
use crate::models::download_item::DownloadRecord;
use crate::models::download_status::DownloadState;
use crate::services::file_naming;

use super::worker::{DownloadWorker, EngineEvent};

/// Hard upper bound for concurrent downloads (mirrors the settings clamp).
const MAX_SIMULTANEOUS_LIMIT: u32 = 10;

/// Atomic set of download ids that currently own a concurrency slot.
#[derive(Debug, Default)]
pub struct QueueScheduler {
    running: Mutex<HashSet<i64>>,
}

impl QueueScheduler {
    /// Empty scheduler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim a slot for `download_id` when fewer than `max` are running.
    ///
    /// `max` is clamped to at least one so a misconfigured zero can never
    /// deadlock the queue.
    pub fn try_acquire(&self, download_id: i64, max: usize) -> bool {
        let mut running = lock(&self.running);
        if running.len() >= max.max(1) {
            return false;
        }
        running.insert(download_id)
    }

    /// Free the slot held by `download_id` (idempotent).
    pub fn release(&self, download_id: i64) {
        lock(&self.running).remove(&download_id);
    }

    /// True when `download_id` currently owns a slot.
    pub fn is_running(&self, download_id: i64) -> bool {
        lock(&self.running).contains(&download_id)
    }

    /// Running ids in ascending order (used for deterministic shutdown).
    pub fn pending(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = lock(&self.running).iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Number of occupied slots.
    pub fn running_count(&self) -> usize {
        lock(&self.running).len()
    }
}

/// State shared by every clone of a [`DownloadManager`].
struct ManagerInner {
    settings: Settings,
    repository: Arc<DownloadRepository>,
    scheduler: Arc<QueueScheduler>,
    events: mpsc::UnboundedSender<EngineEvent>,
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<EngineEvent>>>,
    tasks: Mutex<HashMap<i64, JoinHandle<()>>>,
    cancels: Mutex<HashMap<i64, CancellationToken>>,
    pauses: Mutex<HashMap<i64, Arc<AtomicBool>>>,
}

impl ManagerInner {
    fn max_simultaneous(&self) -> usize {
        self.settings
            .max_simultaneous
            .clamp(1, MAX_SIMULTANEOUS_LIMIT) as usize
    }

    /// Release a finished download's slot and start whatever is queued next.
    fn finish(self: &Arc<Self>, download_id: i64) {
        self.scheduler.release(download_id);
        lock(&self.cancels).remove(&download_id);
        lock(&self.pauses).remove(&download_id);
        // Dropping our own JoinHandle detaches the finished task.
        lock(&self.tasks).remove(&download_id);
        self.pump();
    }

    /// Start queued downloads until the concurrency cap is reached.
    fn pump(self: &Arc<Self>) {
        loop {
            if self.scheduler.running_count() >= self.max_simultaneous() {
                return;
            }
            let queued = match self.repository.list_by_state(&[DownloadState::Queued]) {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(error = %error, "could not list queued downloads");
                    return;
                }
            };
            let Some(record) = queued.into_iter().next() else {
                return;
            };
            if !self.start_record(record) {
                return;
            }
        }
    }

    /// Spawn one worker. Returns `false` when no slot is available.
    fn start_record(self: &Arc<Self>, mut record: DownloadRecord) -> bool {
        let Some(download_id) = record.id else {
            tracing::warn!("refusing to start a download without a database id");
            return false;
        };
        if !self
            .scheduler
            .try_acquire(download_id, self.max_simultaneous())
        {
            return false;
        }

        let cancel = CancellationToken::new();
        let pause = Arc::new(AtomicBool::new(false));
        lock(&self.cancels).insert(download_id, cancel.clone());
        lock(&self.pauses).insert(download_id, Arc::clone(&pause));

        // A retried row may still carry a stale failure summary; workers only
        // clear it on success, so normalise the starting point here.
        record.error_summary = None;
        let worker = DownloadWorker::new(
            record,
            self.settings.clone(),
            Some(Arc::clone(&self.repository)),
            Some(self.events.clone()),
            cancel,
            pause,
            None,
        );

        // Hold the task map while spawning so a very fast worker cannot call
        // `finish` before its handle is registered.
        let mut tasks = lock(&self.tasks);
        let inner = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let _ = worker.run().await;
            inner.finish(download_id);
        });
        tasks.insert(download_id, handle);
        true
    }
}

/// Owns the runtime side of the queue and the shared event stream.
#[derive(Clone)]
pub struct DownloadManager {
    inner: Arc<ManagerInner>,
}

impl DownloadManager {
    /// Build a manager over an existing repository.
    pub fn new(settings: Settings, repository: Arc<DownloadRepository>) -> Self {
        let (events, event_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(ManagerInner {
            settings,
            repository,
            scheduler: Arc::new(QueueScheduler::new()),
            events,
            event_rx: Mutex::new(Some(event_rx)),
            tasks: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            pauses: Mutex::new(HashMap::new()),
        });
        Self { inner }
    }

    /// Take the event receiver (progress + state changes).
    ///
    /// Only one subscriber is supported; subsequent calls return a channel
    /// that never yields events, matching the single-consumer UI loop.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<EngineEvent> {
        let mut guard = lock(&self.inner.event_rx);
        match guard.take() {
            Some(receiver) => receiver,
            None => {
                let (_sender, receiver) = mpsc::unbounded_channel();
                receiver
            }
        }
    }

    /// Effective concurrency cap.
    pub fn max_simultaneous(&self) -> usize {
        self.inner.max_simultaneous()
    }

    /// The settings snapshot used for spawned workers.
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// Insert a queued download and return the persisted record.
    ///
    /// `display_name` defaults to the filename derived from `url`.
    /// `destination_dir` defaults to `settings.default_destination_dir`.
    /// `checksum_sha256` accepts a 64-character hex digest with an optional
    /// `sha256:` prefix.
    pub fn add_download(
        &self,
        url: &str,
        display_name: Option<&str>,
        destination_dir: Option<&Path>,
        checksum_sha256: Option<&str>,
    ) -> Result<DownloadRecord, String> {
        let checksum = normalize_checksum(checksum_sha256)?;

        let directory = match destination_dir {
            Some(dir) => dir.to_path_buf(),
            None if !self
                .inner
                .settings
                .default_destination_dir
                .trim()
                .is_empty() =>
            {
                PathBuf::from(&self.inner.settings.default_destination_dir)
            }
            None => {
                return Err(
                    "no destination directory given and settings.default_destination_dir is empty"
                        .to_string(),
                );
            }
        };

        let filename = file_naming::filename_from_url(url);
        let destination = file_naming::resolve_destination(&directory, &filename);
        let Some(resolved) =
            file_naming::unique_path(&destination, self.inner.settings.filename_collision)
        else {
            let name = destination
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or(filename);
            return Err(format!(
                "destination {name:?} already exists and the collision policy is 'skip'; \
                 choose another filename or policy"
            ));
        };

        let name = display_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                resolved
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
            })
            .unwrap_or(filename);

        let mut record = DownloadRecord::new_queued(url, name, resolved.to_string_lossy());
        record.checksum_sha256 = checksum;
        self.inner
            .repository
            .add(&mut record)
            .map_err(|error| error.to_string())?;
        Ok(record)
    }

    /// Start queued downloads up to the concurrency cap; returns the number
    /// of workers actually spawned.
    pub fn start_all(&self) -> usize {
        let queued = match self
            .inner
            .repository
            .list_by_state(&[DownloadState::Queued])
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "could not list queued downloads");
                return 0;
            }
        };
        let mut started = 0;
        for record in queued {
            if self.inner.scheduler.running_count() >= self.max_simultaneous() {
                break;
            }
            if self.inner.start_record(record) {
                started += 1;
            } else {
                break;
            }
        }
        started
    }

    /// Ask a running worker to pause, or park a queued row.
    pub fn pause(&self, download_id: i64) -> Result<(), String> {
        let record = self.load(download_id)?;
        if self.inner.scheduler.is_running(download_id) {
            if let Some(flag) = lock(&self.inner.pauses).get(&download_id) {
                flag.store(true, Ordering::SeqCst);
            }
            return Ok(());
        }
        match record.state {
            DownloadState::Queued | DownloadState::Interrupted => self
                .inner
                .repository
                .update_state(download_id, DownloadState::Paused)
                .map_err(|error| error.to_string()),
            DownloadState::Paused => Ok(()),
            state => Err(format!(
                "download {download_id} is {}; only queued or running downloads can be paused",
                state.as_str()
            )),
        }
    }

    /// Queue a paused/interrupted/queued row again and start it if a slot is free.
    pub fn resume(&self, download_id: i64) -> Result<(), String> {
        let record = self.load(download_id)?;
        if self.inner.scheduler.is_running(download_id) {
            return Err(format!(
                "download {download_id} is currently running; pause or cancel it first"
            ));
        }
        if !matches!(
            record.state,
            DownloadState::Paused | DownloadState::Interrupted | DownloadState::Queued
        ) {
            return Err(format!(
                "download {download_id} is {}; only paused, interrupted or queued downloads can be resumed",
                record.state.as_str()
            ));
        }
        self.prepare_for_start(download_id)?;
        self.inner.pump();
        Ok(())
    }

    /// Reset a failed/cancelled/interrupted row and queue it again.
    pub fn retry(&self, download_id: i64) -> Result<(), String> {
        let record = self.load(download_id)?;
        if self.inner.scheduler.is_running(download_id) {
            return Err(format!(
                "download {download_id} is currently running; pause or cancel it first"
            ));
        }
        if !matches!(
            record.state,
            DownloadState::Failed
                | DownloadState::Cancelled
                | DownloadState::Interrupted
                | DownloadState::Paused
                | DownloadState::Queued
        ) {
            return Err(format!(
                "download {download_id} is {}; only failed, cancelled, interrupted, paused or \
                 queued downloads can be retried",
                record.state.as_str()
            ));
        }
        self.inner
            .repository
            .update_state(download_id, DownloadState::Queued)
            .map_err(|error| error.to_string())?;
        // Clear the previous attempt's bookkeeping before the worker reloads it.
        if let Some(mut fresh) = self
            .inner
            .repository
            .get(download_id)
            .map_err(|error| error.to_string())?
        {
            fresh.error_summary = None;
            fresh.retry_count = 0;
            fresh.checksum_verified = false;
            self.inner
                .repository
                .update(&fresh)
                .map_err(|error| error.to_string())?;
        }
        self.inner.pump();
        Ok(())
    }

    /// Cancel a running worker or a not-yet-started row.
    pub fn cancel(&self, download_id: i64) -> Result<(), String> {
        let record = self.load(download_id)?;
        if let Some(token) = lock(&self.inner.cancels).get(&download_id) {
            token.cancel();
            return Ok(());
        }
        if record.state.is_terminal() {
            return Ok(());
        }
        self.inner
            .repository
            .update_state(download_id, DownloadState::Cancelled)
            .map_err(|error| error.to_string())
    }

    /// Delete a row; refuses while a worker is still running.
    pub fn remove(&self, download_id: i64) -> Result<bool, String> {
        self.load(download_id)?;
        if self.inner.scheduler.is_running(download_id) {
            return Err(format!(
                "download {download_id} is actively downloading; cancel it before removing"
            ));
        }
        self.inner
            .repository
            .delete(download_id)
            .map_err(|error| error.to_string())
    }

    /// Cancel everything and wait for all workers to settle.
    pub async fn shutdown(&self) {
        let tokens: Vec<CancellationToken> = lock(&self.inner.cancels).values().cloned().collect();
        for token in tokens {
            token.cancel();
        }
        let handles: Vec<JoinHandle<()>> = lock(&self.inner.tasks)
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in handles {
            let _ = handle.await;
        }
    }

    fn load(&self, download_id: i64) -> Result<DownloadRecord, String> {
        self.inner
            .repository
            .get(download_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("unknown download id {download_id}"))
    }

    fn prepare_for_start(&self, download_id: i64) -> Result<(), String> {
        lock(&self.inner.cancels).remove(&download_id);
        lock(&self.inner.pauses).remove(&download_id);
        self.inner
            .repository
            .update_state(download_id, DownloadState::Queued)
            .map_err(|error| error.to_string())
    }
}

/// Lock a mutex, recovering from poisoning instead of panicking.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Normalise a SHA-256 checksum or explain why it is unusable.
fn normalize_checksum(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let lowered = raw.trim().to_ascii_lowercase();
    let hex = lowered
        .strip_prefix("sha256:")
        .map(str::trim)
        .unwrap_or(lowered.as_str());
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(Some(hex.to_string()))
    } else {
        Err(
            "checksum must be a 64-character hex SHA-256 digest, optionally prefixed with \
             'sha256:'"
                .to_string(),
        )
    }
}

//! Backend actor: owns the tokio runtime, the engine and the database.
//!
//! The UI thread never performs I/O. It sends [`Command`]s over an unbounded
//! tokio channel and drains [`UiEvent`]s from a plain `std::sync::mpsc`
//! receiver once per frame. Everything that blocks or awaits lives on the
//! runtime owned by [`AppBackend`], which is dropped (and joined with a
//! timeout) when the window closes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::state::{AddOutcome, DashboardStats};
use crate::config::Settings;
use crate::database::connection::Database;
use crate::database::repository::{DownloadRepository, HistoryQuery};
use crate::engine::manager::DownloadManager;
use crate::engine::progress::DownloadProgress;
use crate::engine::worker::EngineEvent;
use crate::models::discovered_link::{DiscoveryCandidate, DiscoveryResult};
use crate::models::download_item::DownloadRecord;
use crate::services::discovery_planner::{execute_queue_plan, plan_discovery_queue};
use crate::services::link_discovery::LinkDiscovery;
use crate::services::url_intake::{ParseResult, parse_url_lines};

/// Safety valve for the clearing loop; the repository deletes at most 100 rows
/// per call, so a 1,000-pass cap still allows 100,000 rows.
const MAX_CLEAR_PASSES: usize = 1_000;

/// Where the SQLite file lives, given a `%APPDATA%` value.
///
/// Pure so it can be tested without mutating the process environment
/// (`std::env::set_var` is `unsafe` in edition 2024 and would race between
/// parallel tests).
pub fn resolve_database_path(appdata: Option<&str>) -> PathBuf {
    match appdata.map(str::trim).filter(|value| !value.is_empty()) {
        Some(appdata) => PathBuf::from(appdata)
            .join("BulkDownloadManager")
            .join("downloads.db"),
        // No usable profile directory: keep the database beside the binary's
        // working directory rather than failing to start.
        None => PathBuf::from(".").join("downloads.db"),
    }
}

/// The production database location: `%APPDATA%\BulkDownloadManager\downloads.db`.
pub fn default_database_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").ok();
    resolve_database_path(appdata.as_deref())
}

/// Startup inputs.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub database_path: PathBuf,
}

/// Synchronous startup snapshot, returned before any command is sent.
pub struct Startup {
    pub settings: Settings,
    /// Set when the stored settings could not be read; defaults are used.
    pub settings_error: Option<String>,
    /// Rows that were still running when the app last closed.
    pub recovered: Vec<DownloadRecord>,
}

/// Startup snapshot delivered as an event as well, so an already-built shell
/// can refresh itself without reaching into the backend.
#[derive(Debug, Clone)]
pub struct StartupInfo {
    pub settings: Settings,
    pub settings_error: Option<String>,
    pub recovered: Vec<DownloadRecord>,
}

/// Per-row controls the Queue page can issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Pause,
    Resume,
    Retry,
    Cancel,
    Remove,
}

impl QueueAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => "Paused",
            Self::Resume => "Resumed",
            Self::Retry => "Retried",
            Self::Cancel => "Cancelled",
            Self::Remove => "Removed",
        }
    }
}

/// Everything the UI can ask the backend to do.
pub enum Command {
    AddUrls {
        text: String,
        destination: Option<PathBuf>,
        filename: Option<String>,
        checksum: Option<String>,
        start_now: bool,
    },
    PreviewUrls {
        text: String,
    },
    ScanPage {
        url: String,
    },
    CancelScan,
    EnqueueCandidates {
        candidates: Vec<DiscoveryCandidate>,
        destination: PathBuf,
    },
    QueueAction {
        ids: Vec<i64>,
        action: QueueAction,
    },
    StartAll,
    ClearHistory {
        keep_files: bool,
    },
    SaveSettings {
        settings: Settings,
    },
    LoadHistory {
        query: HistoryQuery,
    },
    LoadQueue,
    RefreshDashboard,
    Shutdown,
}

/// Counts from a batch add, so the UI never has to hold engine records.
#[derive(Debug, Default, Clone)]
pub struct BatchReport {
    pub added: usize,
    /// `(label, reason)` pairs for everything that did not make it.
    pub failed: Vec<(String, String)>,
    pub started: usize,
}

impl BatchReport {
    /// Convert into the UI-facing mirror used by the Add Downloads page.
    pub fn into_outcome(self) -> AddOutcome {
        AddOutcome {
            added: self.added,
            failed: self.failed,
            started: self.started,
        }
    }
}

/// Everything the backend pushes back to the UI thread.
pub enum UiEvent {
    Progress(DownloadProgress),
    StateChanged(Box<DownloadRecord>),
    StartupInfo(StartupInfo),
    QueueLoaded {
        records: Vec<DownloadRecord>,
        error: Option<String>,
    },
    HistoryLoaded {
        records: Vec<DownloadRecord>,
        error: Option<String>,
    },
    DashboardLoaded {
        stats: DashboardStats,
        recent: Vec<DownloadRecord>,
    },
    ScanStatus(String),
    ScanFinished(Box<DiscoveryResult>),
    AddPreview(Box<ParseResult>),
    AddResult(BatchReport),
    DiscoverQueued(BatchReport),
    Started(usize),
    ActionFinished {
        label: String,
        errors: Vec<String>,
    },
    SettingsSaved {
        result: Result<Settings, String>,
    },
    HistoryCleared {
        removed: usize,
        error: Option<String>,
    },
    Error(String),
}

/// Owns the runtime and the actor task.
pub struct AppBackend {
    runtime: Option<Runtime>,
    commands: tokio_mpsc::UnboundedSender<Command>,
    actor: Option<JoinHandle<()>>,
}

impl AppBackend {
    /// Open the database, build the engine and start the actor.
    ///
    /// Startup I/O is synchronous and one-off, which is why it is allowed on
    /// the calling thread: the window is not painting yet.
    pub fn start(
        config: BackendConfig,
        events: Sender<UiEvent>,
    ) -> Result<(Self, Startup), String> {
        let database = Database::init_db(&config.database_path).map_err(|error| {
            format!(
                "could not open the database at {}: {error}",
                config.database_path.display()
            )
        })?;
        let repository = Arc::new(DownloadRepository::new(Arc::new(database)));

        // A corrupt settings row is recoverable: fall back to defaults and let
        // the shell surface the warning instead of refusing to start.
        let (settings, settings_error) = match repository.get_settings() {
            Ok(Some(settings)) => (settings, None),
            Ok(None) => (Settings::default(), None),
            Err(error) => (
                Settings::default(),
                Some(format!("could not load the saved settings: {error}")),
            ),
        };

        let recovered = match repository.recover_interrupted() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(error = %error, "could not reconcile interrupted downloads");
                Vec::new()
            }
        };

        let manager = DownloadManager::new(settings.clone(), Arc::clone(&repository));
        let engine_events = manager.subscribe();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("bdm-worker")
            .build()
            .map_err(|error| format!("could not start the async runtime: {error}"))?;

        let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();

        let forward_tx = events.clone();
        runtime.spawn(async move { forward_engine_events(engine_events, forward_tx).await });

        let actor = Actor {
            repository,
            manager,
            settings: settings.clone(),
            events: events.clone(),
            scan_cancel: None,
        };
        let actor_handle = runtime.spawn(async move { actor.run(command_rx).await });

        // The shell uses the returned `Startup`; this event keeps any later
        // subscriber in sync with the same snapshot.
        let _ = events.send(UiEvent::StartupInfo(StartupInfo {
            settings: settings.clone(),
            settings_error: settings_error.clone(),
            recovered: recovered.clone(),
        }));

        Ok((
            Self {
                runtime: Some(runtime),
                commands: command_tx,
                actor: Some(actor_handle),
            },
            Startup {
                settings,
                settings_error,
                recovered,
            },
        ))
    }

    /// Queue a command; a closed channel means the actor already stopped.
    pub fn send(&self, command: Command) {
        if let Err(error) = self.commands.send(command) {
            tracing::warn!(error = %error, "backend actor is no longer running");
        }
    }

    /// Stop the actor and wait for workers to settle, bounded so a stuck
    /// transfer can never block application exit.
    ///
    /// Blocking is only legal outside an async context. If this backend is
    /// dropped from inside another runtime (as the tests do), `block_on` and
    /// `shutdown_timeout` would panic, so the runtime is asked to drain in the
    /// background instead.
    fn stop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let actor = self.actor.take();

        if tokio::runtime::Handle::try_current().is_ok() {
            if let Some(actor) = actor {
                actor.abort();
            }
            runtime.shutdown_background();
            return;
        }

        if let Some(actor) = actor {
            // A timed-out actor is abandoned by design; the process is exiting.
            //
            // The timer must be created inside `block_on`: `tokio::time`
            // registers it with the *ambient* runtime at construction time,
            // and the GUI thread that drops us has none. Building the timeout
            // as `block_on`'s argument is too early — arguments are evaluated
            // before `block_on` enters the runtime context.
            drop(
                runtime
                    .block_on(async { tokio::time::timeout(Duration::from_secs(5), actor).await }),
            );
        }
        runtime.shutdown_timeout(Duration::from_secs(2));
    }
}

impl Drop for AppBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Re-emit engine events as UI events until the engine channel closes.
async fn forward_engine_events(
    mut engine_events: tokio_mpsc::UnboundedReceiver<EngineEvent>,
    events: Sender<UiEvent>,
) {
    while let Some(event) = engine_events.recv().await {
        let ui_event = match event {
            EngineEvent::Progress(progress) => UiEvent::Progress(progress),
            EngineEvent::StateChanged(record) => UiEvent::StateChanged(record),
        };
        if events.send(ui_event).is_err() {
            break;
        }
    }
}

/// Single-owner task that turns commands into engine calls.
struct Actor {
    repository: Arc<DownloadRepository>,
    manager: DownloadManager,
    settings: Settings,
    events: Sender<UiEvent>,
    scan_cancel: Option<CancellationToken>,
}

impl Actor {
    async fn run(mut self, mut commands: tokio_mpsc::UnboundedReceiver<Command>) {
        while let Some(command) = commands.recv().await {
            if !self.handle(command).await {
                break;
            }
        }
        // Bounded by the runtime shutdown in `AppBackend::stop`.
        self.manager.shutdown().await;
    }

    /// Returns `false` when the actor should stop.
    async fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Shutdown => return false,
            Command::AddUrls {
                text,
                destination,
                filename,
                checksum,
                start_now,
            } => {
                self.add_urls(text, destination, filename, checksum, start_now)
                    .await
            }
            Command::PreviewUrls { text } => match self.parse_urls(text).await {
                Ok(parsed) => self.emit(UiEvent::AddPreview(Box::new(parsed))),
                Err(message) => self.emit(UiEvent::Error(message)),
            },
            Command::ScanPage { url } => self.start_scan(url),
            Command::CancelScan => self.cancel_scan(),
            Command::EnqueueCandidates {
                candidates,
                destination,
            } => self.enqueue_candidates(candidates, destination),
            Command::QueueAction { ids, action } => self.run_queue_action(&ids, action),
            Command::StartAll => {
                let started = self.manager.start_all();
                self.emit(UiEvent::Started(started));
                self.refresh_queue();
            }
            Command::ClearHistory { keep_files } => self.clear_history(keep_files),
            Command::SaveSettings { settings } => self.save_settings(settings),
            Command::LoadHistory { query } => self.load_history(query),
            Command::LoadQueue => self.refresh_queue(),
            Command::RefreshDashboard => self.refresh_dashboard(),
        }
        true
    }

    /// Send to the UI; a disconnected receiver only means the window closed.
    fn emit(&self, event: UiEvent) {
        let _ = self.events.send(event);
    }

    // ---- intake -----------------------------------------------------------

    /// DNS resolution is blocking, so it runs on the blocking pool.
    async fn parse_urls(&self, text: String) -> Result<ParseResult, String> {
        let allow_private = self.settings.allow_private_addresses;
        tokio::task::spawn_blocking(move || parse_url_lines(&text, allow_private))
            .await
            .map_err(|error| format!("URL parsing failed: {error}"))
    }

    async fn add_urls(
        &mut self,
        text: String,
        destination: Option<PathBuf>,
        filename: Option<String>,
        checksum: Option<String>,
        start_now: bool,
    ) {
        let parsed = match self.parse_urls(text).await {
            Ok(parsed) => parsed,
            Err(message) => {
                self.emit(UiEvent::Error(message));
                return;
            }
        };

        // Rejections are reported through the same list as add failures so the
        // page has one place to render problems.
        let mut failed = parsed.rejected.clone();
        // A filename/checksum override only makes sense for a single URL, as in
        // the Python dialog; applying one name to a batch would collide.
        let single = parsed.accepted.len() == 1;

        let mut added = 0usize;
        for url in &parsed.accepted {
            let name = if single { filename.as_deref() } else { None };
            match self
                .manager
                .add_download(url, name, destination.as_deref(), checksum.as_deref())
            {
                Ok(_) => added += 1,
                Err(error) => failed.push((url.clone(), error)),
            }
        }

        let started = if start_now && added > 0 {
            self.manager.start_all()
        } else {
            0
        };

        self.emit(UiEvent::AddResult(BatchReport {
            added,
            failed,
            started,
        }));
        self.refresh_queue();
    }

    // ---- discovery --------------------------------------------------------

    fn start_scan(&mut self, url: String) {
        // Only one scan at a time: cancel whatever is still running.
        if let Some(token) = self.scan_cancel.take() {
            token.cancel();
        }
        let token = CancellationToken::new();
        self.scan_cancel = Some(token.clone());

        self.emit(UiEvent::ScanStatus(format!("Scanning {url}…")));

        let settings = self.settings.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let discovery = LinkDiscovery::new();
            let result = discovery.scan(&url, &settings, Some(token.clone())).await;
            if token.is_cancelled() {
                let _ = events.send(UiEvent::ScanStatus("Scan cancelled.".to_string()));
                return;
            }
            let _ = events.send(UiEvent::ScanFinished(Box::new(result)));
        });
    }

    fn cancel_scan(&mut self) {
        match self.scan_cancel.take() {
            Some(token) => {
                token.cancel();
                self.emit(UiEvent::ScanStatus("Scan cancelled.".to_string()));
            }
            None => self.emit(UiEvent::ScanStatus("No scan is running.".to_string())),
        }
    }

    fn enqueue_candidates(&mut self, candidates: Vec<DiscoveryCandidate>, destination: PathBuf) {
        let allow_private = self.settings.allow_private_addresses;
        let existing: Vec<String> = match self.repository.list_all() {
            Ok(rows) => rows.into_iter().map(|record| record.source_url).collect(),
            Err(error) => {
                self.emit(UiEvent::Error(format!(
                    "could not read the existing queue: {error}"
                )));
                return;
            }
        };

        let plan = plan_discovery_queue(&candidates, &existing, allow_private);
        // The planner consumes the plan, so keep its notices first.
        let mut notices = plan.duplicates.clone();
        notices.extend(plan.rejected.clone());

        let manager = self.manager.clone();
        let directory = destination.to_string_lossy().into_owned();
        // `execute_queue_plan` passes (url, destination_dir, filename), which is
        // not `add_download`'s argument order.
        let mut add = |url: &str, dir: &str, name: Option<&str>| {
            manager.add_download(url, name, Some(Path::new(dir)), None)
        };
        let mut started = 0usize;
        let mut start = || {
            started = manager.start_all();
            started
        };

        let report = execute_queue_plan(plan, &directory, &mut add, Some(&mut start));
        notices.extend(report.failed);

        self.emit(UiEvent::DiscoverQueued(BatchReport {
            added: report.added.len(),
            failed: notices,
            started,
        }));
        self.refresh_queue();
    }

    // ---- queue ------------------------------------------------------------

    /// Live rows, oldest id first, matching the scheduler's dispatch order.
    fn queue_rows(&self) -> Result<Vec<DownloadRecord>, String> {
        let mut rows = self
            .repository
            .list_all()
            .map_err(|error| error.to_string())?;
        rows.retain(|record| record.state.is_live());
        rows.sort_by_key(|record| record.id.unwrap_or(i64::MAX));
        Ok(rows)
    }

    fn refresh_queue(&self) {
        match self.queue_rows() {
            Ok(records) => self.emit(UiEvent::QueueLoaded {
                records,
                error: None,
            }),
            // The UI keeps its current rows when `error` is set.
            Err(error) => self.emit(UiEvent::QueueLoaded {
                records: Vec::new(),
                error: Some(error),
            }),
        }
    }

    fn run_queue_action(&mut self, ids: &[i64], action: QueueAction) {
        let mut errors = Vec::new();
        for id in ids {
            let outcome = match action {
                QueueAction::Pause => self.manager.pause(*id),
                QueueAction::Resume => self.manager.resume(*id),
                QueueAction::Retry => self.manager.retry(*id),
                QueueAction::Cancel => self.manager.cancel(*id),
                QueueAction::Remove => self.manager.remove(*id).map(|_| ()),
            };
            if let Err(error) = outcome {
                errors.push(format!("download {id}: {error}"));
            }
        }

        self.emit(UiEvent::ActionFinished {
            label: action.label().to_string(),
            errors,
        });
        self.refresh_queue();
    }

    // ---- history ----------------------------------------------------------

    fn load_history(&self, query: HistoryQuery) {
        match self.repository.history(query) {
            Ok(records) => self.emit(UiEvent::HistoryLoaded {
                records,
                error: None,
            }),
            Err(error) => self.emit(UiEvent::HistoryLoaded {
                records: Vec::new(),
                error: Some(error.to_string()),
            }),
        }
    }

    fn clear_history(&mut self, keep_files: bool) {
        // The repository deletes at most 100 rows per call (a legacy quirk), so
        // "clear everything" needs repeated passes.
        let mut removed = 0usize;
        let mut error = None;
        for _ in 0..MAX_CLEAR_PASSES {
            match self.repository.clear_history(keep_files) {
                Ok(0) => break,
                Ok(batch) => removed += batch,
                Err(failure) => {
                    error = Some(failure.to_string());
                    break;
                }
            }
        }

        self.emit(UiEvent::HistoryCleared { removed, error });
        self.refresh_dashboard();
    }

    // ---- dashboard & settings --------------------------------------------

    fn refresh_dashboard(&self) {
        match self.repository.list_all() {
            Ok(mut rows) => {
                let stats = DashboardStats::from_records(&rows);
                // Newest first, then a short "recent activity" list.
                rows.sort_by(|left, right| right.created_at.cmp(&left.created_at));
                rows.truncate(10);
                self.emit(UiEvent::DashboardLoaded {
                    stats,
                    recent: rows,
                });
            }
            Err(error) => self.emit(UiEvent::Error(format!(
                "could not load the dashboard: {error}"
            ))),
        }
    }

    fn save_settings(&mut self, settings: Settings) {
        match self.repository.save_settings(&settings) {
            Ok(()) => {
                self.settings = settings.clone();
                // Live-apply: the cap changes on the next dispatch and a raised
                // limit starts queued rows immediately.
                self.manager.apply_settings(settings.clone());
                self.emit(UiEvent::SettingsSaved {
                    result: Ok(settings),
                });
            }
            Err(error) => self.emit(UiEvent::SettingsSaved {
                result: Err(error.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::Receiver;
    use std::time::Instant;

    use crate::engine::fake_site::FakeSite;
    use crate::models::download_status::DownloadState;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "bdm-backend-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn settings_for(destination: &Path) -> Settings {
        Settings {
            default_destination_dir: destination.to_string_lossy().into_owned(),
            max_simultaneous: 3,
            connection_timeout: 5.0,
            retry_count: 2,
            retry_delay: 0.01,
            buffer_size: 8192,
            allow_private_addresses: true,
            ..Settings::default()
        }
    }

    /// Serve the fixture from a dedicated thread.
    ///
    /// `FakeSite::spawn` runs its accept loop as a task on the calling runtime,
    /// but these tests block the current-thread test runtime in
    /// `recv_timeout` while the backend downloads from its own runtime. Driving
    /// the server on a private runtime keeps it responsive regardless of what
    /// the test thread is doing.
    fn spawn_fixture() -> Arc<FakeSite> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fixture runtime");
            let site = runtime.block_on(FakeSite::spawn());
            let _ = ready_tx.send(Arc::clone(&site));
            // Keep the accept loop alive for the rest of the test process.
            runtime.block_on(std::future::pending::<()>());
        });
        ready_rx.recv().expect("fixture site failed to start")
    }

    /// Open the backend over `dir` with `settings` already persisted.
    ///
    /// Seeding through the repository (instead of sending `SaveSettings`)
    /// removes the race where the fixture write could land after a test's own
    /// update and overwrite it.
    fn start(dir: &Path, settings: Settings) -> (AppBackend, Startup, Receiver<UiEvent>) {
        {
            let database = Database::init_db(dir.join("downloads.db")).expect("open database");
            let repository = DownloadRepository::new(Arc::new(database));
            repository.save_settings(&settings).expect("seed settings");
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let (backend, startup) = AppBackend::start(
            BackendConfig {
                database_path: dir.join("downloads.db"),
            },
            tx,
        )
        .expect("backend starts");
        (backend, startup, rx)
    }

    /// Wait for the first event matching `predicate`, discarding the rest.
    fn wait_for(
        rx: &Receiver<UiEvent>,
        mut predicate: impl FnMut(&UiEvent) -> bool,
        seconds: u64,
    ) -> UiEvent {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for a UI event");
            match rx.recv_timeout(remaining) {
                Ok(event) => {
                    if predicate(&event) {
                        return event;
                    }
                }
                Err(error) => panic!("no UI event arrived: {error}"),
            }
        }
    }

    fn expect_batch(event: UiEvent) -> BatchReport {
        match event {
            UiEvent::AddResult(report) => report,
            _ => panic!("expected an add result"),
        }
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn resolve_database_path_uses_appdata_then_falls_back() {
        assert_eq!(
            resolve_database_path(Some("C:/Users/me/AppData/Roaming")),
            PathBuf::from("C:/Users/me/AppData/Roaming")
                .join("BulkDownloadManager")
                .join("downloads.db")
        );
        assert_eq!(
            resolve_database_path(None),
            PathBuf::from(".").join("downloads.db")
        );
        assert_eq!(
            resolve_database_path(Some("   ")),
            PathBuf::from(".").join("downloads.db")
        );
    }

    #[test]
    fn database_file_name_is_stable() {
        let path = resolve_database_path(Some("C:/data"));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("downloads.db")
        );
    }

    // ---- settings ---------------------------------------------------------

    #[tokio::test]
    async fn saving_settings_persists_and_is_acknowledged() {
        let dir = temp_dir("settings");
        let (backend, startup, rx) = start(&dir, settings_for(&dir));
        assert_eq!(startup.settings.max_simultaneous, 3);
        assert!(startup.recovered.is_empty());

        let updated = Settings {
            max_simultaneous: 7,
            ..startup.settings.clone()
        };
        backend.send(Command::SaveSettings {
            settings: updated.clone(),
        });

        match wait_for(
            &rx,
            |event| matches!(event, UiEvent::SettingsSaved { .. }),
            5,
        ) {
            UiEvent::SettingsSaved { result } => {
                let saved = result.expect("settings save succeeds");
                assert_eq!(saved.max_simultaneous, 7);
            }
            _ => unreachable!(),
        }

        // A second backend over the same file must read the new value.
        let reopened = Database::init_db(dir.join("downloads.db")).expect("reopen database");
        let repository = DownloadRepository::new(Arc::new(reopened));
        let stored = repository.get_settings().expect("read settings");
        assert_eq!(stored.expect("settings exist").max_simultaneous, 7);
    }

    #[tokio::test]
    async fn corrupt_settings_surface_a_startup_warning() {
        let dir = temp_dir("corrupt-settings");
        let database_path = dir.join("downloads.db");
        {
            let database = Database::init_db(&database_path).expect("open database");
            database
                .execute("INSERT INTO settings (id, theme) VALUES (1, 'bogus')", &[])
                .expect("insert corrupt settings");
        }

        let (tx, _rx) = std::sync::mpsc::channel();
        let (_backend, startup) = AppBackend::start(BackendConfig { database_path }, tx)
            .expect("backend still starts on corrupt settings");

        let error = startup
            .settings_error
            .expect("corrupt settings must be surfaced to the shell");
        assert!(
            error.contains("settings"),
            "error should describe the settings row, got {error:?}"
        );
        assert_eq!(
            startup.settings.max_simultaneous,
            Settings::default().max_simultaneous,
            "defaults are used until the user saves again"
        );
    }

    // ---- intake -----------------------------------------------------------

    #[tokio::test]
    async fn preview_rejects_private_addresses_by_default() {
        let dir = temp_dir("preview");
        let mut settings = settings_for(&dir);
        settings.allow_private_addresses = false;
        let (backend, _startup, rx) = start(&dir, settings);

        backend.send(Command::PreviewUrls {
            text: "http://127.0.0.1:8080/file.bin".to_string(),
        });

        match wait_for(&rx, |event| matches!(event, UiEvent::AddPreview(_)), 5) {
            UiEvent::AddPreview(parsed) => {
                assert!(parsed.accepted.is_empty());
                assert_eq!(parsed.rejected.len(), 1);
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn adds_urls_and_starts_them_to_completion() {
        let site = spawn_fixture();
        site.set_file("payload.bin", b"backend-test-payload".to_vec());

        let dir = temp_dir("add");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let (backend, _startup, rx) = start(&dir, settings_for(&destination));

        backend.send(Command::AddUrls {
            text: site.url("/files/payload.bin"),
            destination: Some(destination.clone()),
            filename: None,
            checksum: None,
            start_now: false,
        });

        let report = expect_batch(wait_for(
            &rx,
            |event| matches!(event, UiEvent::AddResult(_)),
            10,
        ));
        assert_eq!(report.added, 1, "failed: {:?}", report.failed);
        assert!(report.failed.is_empty());
        assert_eq!(report.started, 0);

        backend.send(Command::StartAll);
        wait_for(
            &rx,
            |event| {
                matches!(
                    event,
                    UiEvent::StateChanged(record) if record.state == DownloadState::Completed
                )
            },
            30,
        );

        assert!(destination.join("payload.bin").exists());
    }

    #[tokio::test]
    async fn add_without_a_destination_reports_the_failure() {
        let dir = temp_dir("nodest");
        let mut settings = settings_for(&dir);
        settings.default_destination_dir.clear();
        let (backend, _startup, rx) = start(&dir, settings);

        backend.send(Command::AddUrls {
            text: "https://example.com/file.bin".to_string(),
            destination: None,
            filename: None,
            checksum: None,
            start_now: false,
        });

        let report = expect_batch(wait_for(
            &rx,
            |event| matches!(event, UiEvent::AddResult(_)),
            10,
        ));
        assert_eq!(report.added, 0);
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains("destination"));
    }

    // ---- discovery --------------------------------------------------------

    #[tokio::test]
    async fn scans_a_page_then_enqueues_the_selection() {
        let site = spawn_fixture();
        let dir = temp_dir("discover");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let (backend, _startup, rx) = start(&dir, settings_for(&destination));

        backend.send(Command::ScanPage {
            url: site.url("/page"),
        });
        let result = match wait_for(&rx, |event| matches!(event, UiEvent::ScanFinished(_)), 20) {
            UiEvent::ScanFinished(result) => *result,
            _ => unreachable!(),
        };
        assert!(
            !result.candidates.is_empty(),
            "expected candidates, errors: {:?}",
            result.errors
        );

        backend.send(Command::EnqueueCandidates {
            candidates: result.candidates.clone(),
            destination: destination.clone(),
        });
        let report = match wait_for(&rx, |event| matches!(event, UiEvent::DiscoverQueued(_)), 20) {
            UiEvent::DiscoverQueued(report) => report,
            _ => unreachable!(),
        };
        assert!(report.added > 0, "failed: {:?}", report.failed);
    }

    #[tokio::test]
    async fn cancelling_without_a_scan_is_reported_not_fatal() {
        let dir = temp_dir("nocscan");
        let (backend, _startup, rx) = start(&dir, settings_for(&dir));
        backend.send(Command::CancelScan);
        match wait_for(&rx, |event| matches!(event, UiEvent::ScanStatus(_)), 5) {
            UiEvent::ScanStatus(status) => assert!(status.contains("No scan")),
            _ => unreachable!(),
        }
    }

    // ---- queue ------------------------------------------------------------

    #[tokio::test]
    async fn cancelling_a_running_download_through_the_backend() {
        let site = spawn_fixture();
        let dir = temp_dir("cancel");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let (backend, _startup, rx) = start(&dir, settings_for(&destination));

        backend.send(Command::AddUrls {
            text: site.url("/stall"),
            destination: Some(destination.clone()),
            filename: None,
            checksum: None,
            start_now: true,
        });
        let report = expect_batch(wait_for(
            &rx,
            |event| matches!(event, UiEvent::AddResult(_)),
            10,
        ));
        assert_eq!(report.added, 1);
        let id = next_queue_row(&rx);

        backend.send(Command::QueueAction {
            ids: vec![id],
            action: QueueAction::Cancel,
        });

        // `ActionFinished` is emitted by the actor as soon as `cancel` returns,
        // while `StateChanged` travels through the engine event forwarder: the
        // two can arrive in either order, so watch for both at once.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut cancelled = false;
        let mut finished = false;
        while !(cancelled && finished) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for the cancel");
            let event = rx.recv_timeout(remaining).expect("cancel event arrived");
            match event {
                UiEvent::StateChanged(record) if record.state == DownloadState::Cancelled => {
                    cancelled = true;
                }
                UiEvent::ActionFinished { .. } => finished = true,
                _ => {}
            }
        }
    }

    /// Read the id of the single live row from the next `QueueLoaded` event.
    fn next_queue_row(rx: &Receiver<UiEvent>) -> i64 {
        match wait_for(rx, |event| matches!(event, UiEvent::QueueLoaded { .. }), 10) {
            UiEvent::QueueLoaded { records, .. } => records
                .first()
                .and_then(|record| record.id)
                .expect("row id"),
            _ => unreachable!(),
        }
    }

    // ---- recovery ---------------------------------------------------------

    #[tokio::test]
    async fn startup_recovers_interrupted_rows_and_resumes_them() {
        let site = spawn_fixture();
        site.set_file("resume.bin", b"resume-payload".to_vec());

        let dir = temp_dir("recover");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let settings = settings_for(&destination);

        // Seed a row that was still running when the app "closed".
        {
            let database = Database::init_db(dir.join("downloads.db")).expect("open database");
            let repository = DownloadRepository::new(Arc::new(database));
            repository.save_settings(&settings).expect("save settings");
            let mut record = DownloadRecord::new_queued(
                site.url("/files/resume.bin"),
                "resume.bin",
                destination
                    .join("resume.bin")
                    .to_string_lossy()
                    .into_owned(),
            );
            record.state = DownloadState::Downloading;
            repository.add(&mut record).expect("seed row");
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let (backend, startup) = AppBackend::start(
            BackendConfig {
                database_path: dir.join("downloads.db"),
            },
            tx,
        )
        .expect("backend starts");

        assert_eq!(startup.recovered.len(), 1);
        let recovered = &startup.recovered[0];
        assert_eq!(recovered.state, DownloadState::Interrupted);
        let id = recovered.id.expect("recovered id");

        backend.send(Command::QueueAction {
            ids: vec![id],
            action: QueueAction::Resume,
        });
        wait_for(
            &rx,
            |event| {
                matches!(
                    event,
                    UiEvent::StateChanged(record)
                        if record.id == Some(id) && record.state == DownloadState::Completed
                )
            },
            30,
        );
    }

    // ---- shutdown ---------------------------------------------------------

    #[tokio::test]
    async fn shutdown_command_completes_promptly() {
        let dir = temp_dir("shutdown");
        let (backend, _startup, _rx) = start(&dir, settings_for(&dir));
        backend.send(Command::Shutdown);

        let started = Instant::now();
        drop(backend);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "shutdown took {:?}",
            started.elapsed()
        );
    }

    // ---- plain-thread lifecycle (the real application) -------------------

    /// The real application drops the backend on the GUI main thread, where no
    /// Tokio runtime is ambient. Dropping must therefore shut the actor down
    /// without panicking: the timer used to bound the shutdown has to be built
    /// inside the backend's own runtime, not on the caller's thread.
    ///
    /// Every other test in this module is a `#[tokio::test]`, so they all take
    /// the nested-runtime branch in `stop()` and never exercised this path.
    #[test]
    fn dropping_the_backend_on_a_plain_thread_shuts_down_cleanly() {
        let site = spawn_fixture();
        site.set_file("plain.bin", b"plain-thread-payload".to_vec());

        let dir = temp_dir("plain-drop");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let (backend, _startup, rx) = start(&dir, settings_for(&destination));

        // A normal session: queue, download and discovery all run.
        backend.send(Command::LoadQueue);
        wait_for(
            &rx,
            |event| matches!(event, UiEvent::QueueLoaded { .. }),
            10,
        );

        backend.send(Command::AddUrls {
            text: site.url("/files/plain.bin"),
            destination: Some(destination.clone()),
            filename: None,
            checksum: None,
            start_now: true,
        });
        let report = expect_batch(wait_for(
            &rx,
            |event| matches!(event, UiEvent::AddResult(_)),
            10,
        ));
        assert_eq!(report.added, 1, "failed: {:?}", report.failed);

        backend.send(Command::ScanPage {
            url: site.url("/page"),
        });
        wait_for(&rx, |event| matches!(event, UiEvent::ScanFinished(_)), 20);

        // This is the operation that panicked with "there is no reactor
        // running" when the timer was constructed outside `block_on`.
        drop(backend);
    }

    #[test]
    fn dropping_the_backend_during_a_download_and_scan_is_bounded() {
        let site = spawn_fixture();
        let dir = temp_dir("plain-active");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let (backend, _startup, rx) = start(&dir, settings_for(&destination));

        backend.send(Command::AddUrls {
            text: site.url("/stall"),
            destination: Some(destination.clone()),
            filename: None,
            checksum: None,
            start_now: true,
        });
        let report = expect_batch(wait_for(
            &rx,
            |event| matches!(event, UiEvent::AddResult(_)),
            10,
        ));
        assert_eq!(report.added, 1, "failed: {:?}", report.failed);

        backend.send(Command::ScanPage {
            url: site.url("/page"),
        });

        let started = Instant::now();
        drop(backend);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "shutdown took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn failed_download_only_marks_the_row_and_emits_events() {
        let site = spawn_fixture();
        let dir = temp_dir("plain-failure");
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("create destination");
        let (backend, _startup, rx) = start(&dir, settings_for(&destination));

        backend.send(Command::AddUrls {
            text: site.url("/status/404"),
            destination: Some(destination.clone()),
            filename: None,
            checksum: None,
            start_now: true,
        });
        let report = expect_batch(wait_for(
            &rx,
            |event| matches!(event, UiEvent::AddResult(_)),
            10,
        ));
        assert_eq!(report.added, 1, "failed: {:?}", report.failed);

        let record = match wait_for(
            &rx,
            |event| matches!(event, UiEvent::StateChanged(record) if record.state == DownloadState::Failed),
            20,
        ) {
            UiEvent::StateChanged(record) => *record,
            _ => unreachable!(),
        };
        assert!(
            record.error_summary.is_some(),
            "a failed row keeps its reason"
        );

        // The runtime survives the failure: a later command still works.
        backend.send(Command::LoadQueue);
        wait_for(
            &rx,
            |event| matches!(event, UiEvent::QueueLoaded { .. }),
            10,
        );

        drop(backend);
    }
}

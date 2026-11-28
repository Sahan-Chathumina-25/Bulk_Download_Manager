//! UI-facing state for the Dioxus shell.
//!
//! All side effects go through [`backend::AppBackend`]. The Dioxus component
//! only renders this model and sends commands; the backend pushes `UiEvent`s
//! into a channel that the root component drains on a short timer.

use std::sync::{Arc, Mutex, mpsc::TryRecvError};
use std::time::{Duration, Instant};

use crate::app::backend::{AppBackend, Command, QueueAction, Startup, UiEvent};
use crate::app::state::{
    AddDownloadsState, AiFinderState, Confirmation, DashboardState, DiscoveryState, HistoryState,
    Notifications, Page, QueueState, SettingsState, Severity,
};
use crate::config::Settings;
use crate::models::download_status::{DownloadProgress, DownloadState};

/// The whole reactive state behind one `Signal<UiModel>`.
pub struct UiModel {
    pub backend: Arc<AppBackend>,
    pub events: Arc<Mutex<std::sync::mpsc::Receiver<UiEvent>>>,
    pub events_connected: bool,
    pub settings: Settings,
    pub page: Page,
    pub queue: QueueState,
    pub history: HistoryState,
    pub discovery: DiscoveryState,
    pub ai: AiFinderState,
    pub add: AddDownloadsState,
    pub settings_state: SettingsState,
    pub dashboard: DashboardState,
    pub notifications: Notifications,
    pub confirmation: Option<Confirmation>,
    pub settings_error: Option<String>,
    pub discovery_destination: Option<std::path::PathBuf>,
    pub ai_destination: Option<std::path::PathBuf>,
    pub add_line_count: usize,
    pub history_dirty: bool,
    pub dashboard_dirty: bool,
    pub quit_requested: bool,
    pub recovered_prompted: bool,
    pub last_tick: Instant,

    pub add_urls: String,
    pub add_filename: String,
    pub add_checksum: String,
    pub discovery_url: String,
    pub ai_urls: String,
    pub history_search: String,

    pub set_destination: String,
    pub set_max_simultaneous: String,
    pub set_timeout: String,
    pub set_retry_count: String,
    pub set_retry_delay: String,
    pub set_buffer_size: String,
    pub set_bandwidth: String,
    pub set_user_agent: String,
    pub set_extensions: String,
    pub set_ad_hosts: String,
}

impl UiModel {
    pub fn new(
        backend: Arc<AppBackend>,
        startup: Arc<Startup>,
        events: Arc<Mutex<std::sync::mpsc::Receiver<UiEvent>>>,
    ) -> Self {
        let mut settings_state = SettingsState::default();
        settings_state.load(startup.settings.clone());

        let recovered = startup.recovered.len();
        let confirmation = (recovered > 0).then_some(Confirmation::ResumeInterrupted {
            count: recovered,
        });

        let mut model = Self {
            backend,
            events,
            events_connected: true,
            settings: startup.settings.clone(),
            page: Page::Dashboard,
            queue: QueueState::default(),
            history: HistoryState::default(),
            discovery: DiscoveryState::default(),
            ai: AiFinderState::default(),
            add: AddDownloadsState::new(),
            settings_state,
            dashboard: DashboardState::default(),
            notifications: Notifications::default(),
            confirmation,
            settings_error: startup.settings_error.clone(),
            discovery_destination: None,
            ai_destination: None,
            add_line_count: 0,
            history_dirty: true,
            dashboard_dirty: true,
            quit_requested: false,
            recovered_prompted: recovered > 0,
            last_tick: Instant::now(),
            add_urls: String::new(),
            add_filename: String::new(),
            add_checksum: String::new(),
            discovery_url: String::new(),
            ai_urls: String::new(),
            history_search: String::new(),
            set_destination: String::new(),
            set_max_simultaneous: String::new(),
            set_timeout: String::new(),
            set_retry_count: String::new(),
            set_retry_delay: String::new(),
            set_buffer_size: String::new(),
            set_bandwidth: String::new(),
            set_user_agent: String::new(),
            set_extensions: String::new(),
            set_ad_hosts: String::new(),
        };

        model.sync_settings_inputs();
        model.send(Command::LoadQueue);
        model.send(Command::RefreshDashboard);
        model
    }

    pub fn send(&self, command: Command) {
        self.backend.send(command);
    }

    pub fn toast(&mut self, severity: Severity, text: impl Into<String>) {
        self.notifications.push(severity, text);
    }

    /// Refresh the settings text fields from the persisted settings.
    pub fn sync_settings_inputs(&mut self) {
        let settings = &self.settings;
        self.set_destination = settings.default_destination_dir.clone();
        self.set_max_simultaneous = settings.max_simultaneous.to_string();
        self.set_timeout = settings.connection_timeout.to_string();
        self.set_retry_count = settings.retry_count.to_string();
        self.set_retry_delay = settings.retry_delay.to_string();
        self.set_buffer_size = settings.buffer_size.to_string();
        self.set_bandwidth = (settings.bandwidth_limit_bps as f64 / 1024.0).to_string();
        self.set_user_agent = settings.user_agent.clone();
        self.set_extensions = settings
            .allowed_extensions
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        self.set_ad_hosts = settings.ad_filter_list_path.clone();
        self.settings_state.load(settings.clone());
    }

    pub fn request_history(&mut self) {
        self.history.loading = true;
        self.history_dirty = false;
        self.send(Command::LoadHistory {
            query: self.history.to_query(),
        });
    }

    pub fn request_dashboard(&mut self) {
        self.dashboard_dirty = false;
        self.send(Command::RefreshDashboard);
    }

    pub fn confirm(&mut self, confirmation: Confirmation) {
        self.confirmation = Some(confirmation);
    }

    pub fn switch_page(&mut self, page: Page) {
        if self.page == page {
            return;
        }
        self.page = page;
        match page {
            Page::History => self.request_history(),
            Page::Dashboard => self.request_dashboard(),
            Page::AiFinder => self.send(Command::LoadAnalysis),
            _ => {}
        }
    }

    pub fn active_count(&self) -> usize {
        self.queue
            .records()
            .iter()
            .filter(|record| crate::app::theme::is_active(record.state))
            .count()
    }

    pub fn total_speed(&self) -> f64 {
        self.queue
            .records()
            .iter()
            .filter(|record| crate::app::theme::is_active(record.state))
            .filter_map(|record| self.queue.progress_for(record))
            .map(|progress| progress.speed_bps)
            .sum()
    }

    /// Drain backend events, expire toasts, and refresh dirty data.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let delta = now - self.last_tick;
        self.last_tick = now;
        self.notifications.tick(delta);

        loop {
            let event = {
                let Ok(receiver) = self.events.lock() else {
                    self.events_connected = false;
                    break;
                };
                match receiver.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.events_connected = false;
                        break;
                    }
                }
            };
            self.apply_event(event);
        }

        if self.dashboard_dirty && self.page == Page::Dashboard {
            self.request_dashboard();
        }
        if self.history_dirty && self.page == Page::History {
            self.request_history();
        }
    }

    pub fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Progress(progress) => {
                self.queue.apply_progress(DownloadProgress {
                    download_id: progress.download_id,
                    downloaded_bytes: progress.downloaded_bytes,
                    total_bytes: progress.total_bytes,
                    speed_bps: progress.speed_bps,
                    eta_seconds: progress.eta_seconds,
                });
            }
            UiEvent::StateChanged(record) => {
                let record = *record;
                if record.state.is_terminal() {
                    self.history_dirty = true;
                    self.dashboard_dirty = true;
                }
                self.queue.upsert(record);
            }
            UiEvent::StartupInfo(info) => {
                self.settings = info.settings.clone();
                self.settings_state.load(info.settings);
                if let Some(error) = info.settings_error {
                    self.settings_error = Some(error);
                }
                if !self.recovered_prompted && !info.recovered.is_empty() {
                    self.recovered_prompted = true;
                    self.confirm(Confirmation::ResumeInterrupted {
                        count: info.recovered.len(),
                    });
                }
            }
            UiEvent::QueueLoaded { records, error } => match error {
                Some(message) => self.toast(Severity::Error, message),
                None => self.queue.replace(records),
            },
            UiEvent::HistoryLoaded { records, error } => {
                if let Some(message) = &error {
                    self.toast(Severity::Error, message.clone());
                }
                self.history.apply_loaded(records, error);
            }
            UiEvent::DashboardLoaded { stats, recent } => {
                self.dashboard.stats = stats;
                self.dashboard.recent = recent;
                self.dashboard.error = None;
            }
            UiEvent::ScanStatus(status) => {
                self.discovery.scanning = status.starts_with("Scanning");
                self.discovery.status = status;
            }
            UiEvent::ScanFinished(result) => {
                self.discovery.apply_result(*result);
                self.toast(Severity::Info, "Scan finished.");
            }
            UiEvent::AnalysisStatus(status) => {
                self.ai.analyzing = status.starts_with("Analyzing");
                self.ai.status = status;
            }
            UiEvent::AnalysisFinished(report) => {
                self.ai.apply_report(*report);
                self.toast(Severity::Info, "Analysis finished.");
            }
            UiEvent::AnalysisLoaded(report) => {
                if let Some(report) = report {
                    self.ai.apply_loaded(*report);
                }
            }
            UiEvent::AiQueued(report) => {
                let outcome = report.into_outcome();
                if outcome.added > 0 {
                    self.toast(
                        Severity::Success,
                        format!("{} archive file(s) added to the queue.", outcome.added),
                    );
                }
                if let Some((_, reason)) = outcome.failed.first() {
                    self.toast(Severity::Warning, reason.clone());
                }
                self.ai.outcome = Some(outcome);
            }
            UiEvent::AddPreview(parsed) => {
                self.add.preview = Some(*parsed);
                self.add.preview_pending = false;
            }
            UiEvent::AddResult(report) => {
                let outcome = report.into_outcome();
                if outcome.added > 0 {
                    self.toast(
                        Severity::Success,
                        format!("{} download(s) added.", outcome.added),
                    );
                }
                if let Some((_, reason)) = outcome.failed.first() {
                    self.toast(Severity::Warning, reason.clone());
                }
                self.add.outcome = Some(outcome);
                self.add.add_pending = false;
            }
            UiEvent::DiscoverQueued(report) => {
                let outcome = report.into_outcome();
                if outcome.added > 0 {
                    self.toast(
                        Severity::Success,
                        format!("{} link(s) added to the queue.", outcome.added),
                    );
                }
                if let Some((_, reason)) = outcome.failed.first() {
                    self.toast(Severity::Warning, reason.clone());
                }
                self.discovery.outcome = Some(outcome);
            }
            UiEvent::Started(started) => {
                if started > 0 {
                    self.toast(Severity::Info, format!("Started {started} download(s)."));
                }
            }
            UiEvent::ActionFinished { label, errors } => {
                if errors.is_empty() {
                    self.toast(Severity::Success, format!("{label}."));
                } else {
                    self.toast(Severity::Error, errors.join("; "));
                }
            }
            UiEvent::SettingsSaved { result } => match result {
                Ok(settings) => {
                    self.settings = settings.clone();
                    self.settings_state.mark_saved(settings);
                    self.sync_settings_inputs();
                    self.toast(Severity::Success, "Settings saved.");
                }
                Err(message) => {
                    self.settings_state.set_error(Some(message.clone()));
                    self.toast(Severity::Error, message);
                }
            },
            UiEvent::HistoryCleared { removed, error } => match error {
                Some(message) => self.toast(Severity::Error, message),
                None => self.toast(
                    if removed == 0 {
                        Severity::Info
                    } else {
                        Severity::Success
                    },
                    format!("Cleared {removed} history record(s)."),
                ),
            },
            UiEvent::Error(message) => {
                self.add.error = Some(message.clone());
                self.add.preview_pending = false;
                self.add.add_pending = false;
                self.toast(Severity::Error, message);
            }
        }
    }

    /// True when the caller should close the application.
    pub fn request_quit(&mut self) -> bool {
        if self.quit_requested {
            return true;
        }
        let active = self.active_count();
        if active > 0 {
            self.confirm(Confirmation::QuitWithActive { count: active });
            false
        } else {
            true
        }
    }

    /// Handle the pending modal answer. Returns true when the app should quit.
    pub fn accept_confirmation(&mut self, confirmation: Confirmation) -> bool {
        self.confirmation = None;
        match confirmation {
            Confirmation::ResumeInterrupted { .. } => {
                let ids: Vec<i64> = self
                    .queue
                    .records()
                    .iter()
                    .filter(|record| record.state == DownloadState::Interrupted)
                    .filter_map(|record| record.id)
                    .collect();
                if ids.is_empty() {
                    self.send(Command::StartAll);
                } else {
                    self.send(Command::QueueAction {
                        ids,
                        action: QueueAction::Resume,
                    });
                }
            }
            Confirmation::ClearHistory => {
                let keep_files = !self.history.keep_files_on_clear;
                self.send(Command::ClearHistory { keep_files });
            }
            Confirmation::QuitWithActive { .. } => {
                self.quit_requested = true;
                return true;
            }
            Confirmation::RemoveQueue { ids } => {
                self.send(Command::QueueAction {
                    ids,
                    action: QueueAction::Remove,
                });
            }
            Confirmation::CancelQueue { ids } => {
                self.send(Command::QueueAction {
                    ids,
                    action: QueueAction::Cancel,
                });
            }
        }
        false
    }

    /// Effective destination for a page-level override, else the configured one.
    pub fn destination(&self, override_path: Option<&std::path::PathBuf>) -> std::path::PathBuf {
        if let Some(path) = override_path {
            return path.clone();
        }
        let configured = self.settings.default_destination_dir.trim();
        if configured.is_empty() {
            std::path::PathBuf::from(".")
        } else {
            std::path::PathBuf::from(configured)
        }
    }
}

/// Duration between backend event pumps.
pub const TICK_INTERVAL: Duration = Duration::from_millis(150);

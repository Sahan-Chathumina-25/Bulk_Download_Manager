//! GPUI desktop UI layer.
//!
//! The UI is deliberately split from the download engine: every side effect
//! goes through [`backend::AppBackend`], and the UI only renders state and
//! sends commands. The root view owns all UI state; the backend pushes
//! [`UiEvent`]s into a channel that the view drains on a short timer.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::backend::{AppBackend, Command, QueueAction, Startup, UiEvent};
use crate::app::state::{
    AddDownloadsState, AiFinderState, Confirmation, DashboardState, DiscoveryState, HistoryState,
    Notifications, Page, QueueState, SettingsState, Severity,
};
use crate::app::theme::Palette;
use crate::config::Settings;
use crate::models::download_status::DownloadState;

pub mod add_downloads;
pub mod ai_file_finder;
pub mod backend;
pub mod components;
pub mod dashboard;
pub mod discover;
pub mod format;
pub mod history_view;
pub mod queue_view;
pub mod settings_view;
pub mod state;
pub mod theme;

/// Root GPUI view: owns the backend handle and all UI state.
pub struct App {
    backend: AppBackend,
    events: Receiver<UiEvent>,
    events_connected: bool,
    settings: Settings,
    page: Page,
    queue: QueueState,
    history: HistoryState,
    discovery: DiscoveryState,
    ai: AiFinderState,
    add: AddDownloadsState,
    settings_state: SettingsState,
    dashboard: DashboardState,
    notifications: Notifications,
    confirmation: Option<Confirmation>,
    settings_error: Option<String>,
    discovery_destination: Option<std::path::PathBuf>,
    ai_destination: Option<std::path::PathBuf>,
    add_line_count: usize,
    history_dirty: bool,
    dashboard_dirty: bool,
    quit_requested: bool,
    recovered_prompted: bool,
    last_tick: Instant,

    pub(crate) add_urls: Entity<TextareaState>,
    pub(crate) add_filename: Entity<InputState>,
    pub(crate) add_checksum: Entity<InputState>,
    pub(crate) discovery_url: Entity<InputState>,
    pub(crate) ai_urls: Entity<TextareaState>,
    pub(crate) history_search: Entity<InputState>,

    pub(crate) set_destination: Entity<InputState>,
    pub(crate) set_max_simultaneous: Entity<InputState>,
    pub(crate) set_timeout: Entity<InputState>,
    pub(crate) set_retry_count: Entity<InputState>,
    pub(crate) set_retry_delay: Entity<InputState>,
    pub(crate) set_buffer_size: Entity<InputState>,
    pub(crate) set_bandwidth: Entity<InputState>,
    pub(crate) set_user_agent: Entity<InputState>,
    pub(crate) set_extensions: Entity<InputState>,
    pub(crate) set_ad_hosts: Entity<InputState>,

    _subscriptions: Vec<Subscription>,
}

impl App {
    pub fn new(
        backend: AppBackend,
        startup: Startup,
        events: Receiver<UiEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let add_urls = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(5)
                .placeholder("One download URL per line…")
        });
        let add_filename = cx.new(|cx| InputState::new(window, cx).placeholder("Optional file name"));
        let add_checksum = cx.new(|cx| InputState::new(window, cx).placeholder("sha256:<hex>"));
        let discovery_url = cx.new(|cx| InputState::new(window, cx).placeholder("https://example.com/page"));
        let ai_urls = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .placeholder("Paste URLs to inspect…")
        });
        let history_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search files, paths, URLs"));

        let set_destination = cx.new(|cx| InputState::new(window, cx).placeholder("Default download folder"));
        let set_max_simultaneous = cx.new(|cx| InputState::new(window, cx).placeholder("1–10"));
        let set_timeout = cx.new(|cx| InputState::new(window, cx).placeholder("Seconds"));
        let set_retry_count = cx.new(|cx| InputState::new(window, cx).placeholder("0–20"));
        let set_retry_delay = cx.new(|cx| InputState::new(window, cx).placeholder("Seconds"));
        let set_buffer_size = cx.new(|cx| InputState::new(window, cx).placeholder("Bytes"));
        let set_bandwidth = cx.new(|cx| InputState::new(window, cx).placeholder("KB/s, 0 = unlimited"));
        let set_user_agent = cx.new(|cx| InputState::new(window, cx).placeholder("User-Agent"));
        let set_extensions = cx.new(|cx| InputState::new(window, cx).placeholder("zip, pdf, mp4"));
        let set_ad_hosts = cx.new(|cx| InputState::new(window, cx).placeholder("tracker.example.com"));

        let mut settings_state = SettingsState::default();
        settings_state.load(startup.settings.clone());

        let recovered = startup.recovered.len();
        let confirmation = if recovered > 0 {
            Some(Confirmation::ResumeInterrupted { count: recovered })
        } else {
            None
        };

        let mut app = Self {
            backend,
            events,
            events_connected: true,
            settings: startup.settings,
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
            settings_error: startup.settings_error,
            discovery_destination: None,
            ai_destination: None,
            add_line_count: 0,
            history_dirty: true,
            dashboard_dirty: true,
            quit_requested: false,
            recovered_prompted: recovered > 0,
            last_tick: Instant::now(),
            add_urls,
            add_filename,
            add_checksum,
            discovery_url,
            ai_urls,
            history_search,
            set_destination,
            set_max_simultaneous,
            set_timeout,
            set_retry_count,
            set_retry_delay,
            set_buffer_size,
            set_bandwidth,
            set_user_agent,
            set_extensions,
            set_ad_hosts,
            _subscriptions: Vec::new(),
        };

        app.sync_settings_inputs(window, cx);
        app.send(Command::LoadQueue);
        app.send(Command::RefreshDashboard);

        cx.spawn(|weak: WeakEntity<App>, cx: &mut AsyncApp| {
            let cx = cx.clone();
            async move {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                let alive = cx.update(|app| {
                    weak.update(app, |this, view_cx| {
                        this.tick();
                        view_cx.notify();
                    })
                    .is_ok()
                });
                if !alive {
                    break;
                }
            }
            }
        })
        .detach();

        app
    }

    // ---- helpers used by page views ---------------------------------------

    pub(crate) fn send(&self, command: Command) {
        self.backend.send(command);
    }

    pub(crate) fn toast(&mut self, severity: Severity, text: impl Into<String>) {
        self.notifications.push(severity, text);
    }

    /// Push the current settings into the retained text inputs.
    pub(crate) fn sync_settings_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        let values = [
            (&self.set_destination, settings.default_destination_dir.clone()),
            (&self.set_max_simultaneous, settings.max_simultaneous.to_string()),
            (&self.set_timeout, settings.connection_timeout.to_string()),
            (&self.set_retry_count, settings.retry_count.to_string()),
            (&self.set_retry_delay, settings.retry_delay.to_string()),
            (&self.set_buffer_size, settings.buffer_size.to_string()),
            (
                &self.set_bandwidth,
                (settings.bandwidth_limit_bps as f64 / 1024.0).to_string(),
            ),
            (&self.set_user_agent, settings.user_agent.clone()),
            (
                &self.set_extensions,
                settings
                    .allowed_extensions
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            (&self.set_ad_hosts, settings.ad_filter_list_path.clone()),
        ];
        for (input, value) in values {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.settings_state.load(settings);
    }

    pub(crate) fn request_history(&mut self) {
        self.history.loading = true;
        self.history_dirty = false;
        self.send(Command::LoadHistory {
            query: self.history.to_query(),
        });
    }

    pub(crate) fn request_dashboard(&mut self) {
        self.dashboard_dirty = false;
        self.send(Command::RefreshDashboard);
    }

    pub(crate) fn confirm(&mut self, confirmation: Confirmation) {
        self.confirmation = Some(confirmation);
    }

    pub(crate) fn switch_page(&mut self, page: Page) {
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

    pub(crate) fn active_count(&self) -> usize {
        self.queue
            .records()
            .iter()
            .filter(|record| theme::is_active(record.state))
            .count()
    }

    fn total_speed(&self) -> f64 {
        self.queue
            .records()
            .iter()
            .filter(|record| theme::is_active(record.state))
            .filter_map(|record| self.queue.progress_for(record))
            .map(|progress| progress.speed_bps)
            .sum()
    }

    // ---- event pump -------------------------------------------------------

    fn tick(&mut self) {
        let now = Instant::now();
        let delta = now - self.last_tick;
        self.last_tick = now;
        self.notifications.tick(delta);

        loop {
            match self.events.try_recv() {
                Ok(event) => self.apply_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.events_connected = false;
                    break;
                }
            }
        }

        if self.dashboard_dirty && self.page == Page::Dashboard {
            self.request_dashboard();
        }
        if self.history_dirty && self.page == Page::History {
            self.request_history();
        }
    }

    fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Progress(progress) => {
                self.queue.apply_progress(crate::models::download_status::DownloadProgress {
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

    /// Returns true when the caller should close the application.
    fn request_quit(&mut self) -> bool {
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

    /// Returns true when the caller should close the application.
    fn accept_confirmation(&mut self, confirmation: Confirmation) -> bool {
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
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(window, cx);

        let nav = div()
            .w(px(210.0))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .border_r_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .p(px(12.0))
            .child(
                div()
                    .px(px(8.0))
                    .py(px(10.0))
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(15.0))
                    .child("Bulk Download Manager"),
            )
            .children(Page::ALL.iter().map(|page| {
                let page = *page;
                let active = self.page == page;
                Button::new(("nav", page as usize))
                    .label(page.title())
                    .when(active, |button| button.primary())
                    .when(!active, |button| button.ghost())
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.switch_page(page);
                        cx.notify();
                    }))
            }))
            .child(div().flex_1())
            .child(
                div()
                    .px(px(8.0))
                    .py(px(8.0))
                    .text_size(px(11.0))
                    .text_color(palette.text_muted)
                    .child(if self.events_connected {
                        "Engine connected"
                    } else {
                        "Engine disconnected"
                    }),
            );

        let toolbar = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .border_b_1()
            .border_color(palette.border)
            .bg(palette.window)
            .px(px(20.0))
            .py(px(12.0))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(13.0))
                    .child(self.page.title().to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.0))
                    .text_size(px(12.0))
                    .text_color(palette.text_muted)
                    .child(format!("{} active", self.active_count()))
                    .child(format_speed_label(self.total_speed()))
                    .child(
                        Button::new("quit")
                            .ghost()
                            .label("Quit")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.request_quit() {
                                    cx.quit();
                                }
                            })),
                    ),
            );

        let content = match self.page {
            Page::Dashboard => dashboard::show(self, window, cx).into_any_element(),
            Page::Queue => queue_view::show(self, window, cx).into_any_element(),
            Page::AddDownloads => add_downloads::show(self, window, cx).into_any_element(),
            Page::History => history_view::show(self, window, cx).into_any_element(),
            Page::Discover => discover::show(self, window, cx).into_any_element(),
            Page::Settings => settings_view::show(self, window, cx).into_any_element(),
            Page::AiFinder => ai_file_finder::show(self, window, cx).into_any_element(),
        };

        let mut root = div()
            .flex()
            .size_full()
            .bg(palette.window)
            .text_color(palette.text)
            .child(nav)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(toolbar)
                    .child(
                        div()
                            .flex_1()
                            .overflow_y_scrollbar()
                            .p(px(20.0))
                            .child(content),
                    ),
            );

        root = root.child(self.toast_layer(palette));
        if let Some(confirmation) = self.confirmation.clone() {
            root = root.child(self.confirmation_layer(confirmation, palette, cx));
        }
        root
    }
}

impl App {
    fn toast_layer(&self, palette: Palette) -> Div {
        let items = self.notifications.items();
        div()
            .absolute()
            .top(px(16.0))
            .right(px(16.0))
            .w(px(360.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(items.iter().map(|notification| {
                let color = theme::severity_color(notification.severity, palette);
                div()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(color)
                    .bg(palette.surface)
                    .p(px(12.0))
                    .text_size(px(12.0))
                    .text_color(palette.text)
                    .child(notification.text.clone())
            }))
    }

    fn confirmation_layer(
        &self,
        confirmation: Confirmation,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = confirmation.title().to_string();
        let message = confirmation.message();
        let accept = confirmation.accept_label().to_string();
        let dangerous = confirmation.is_destructive();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000099))
            .child(
                div()
                    .w(px(420.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.surface)
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(15.0))
                            .child(title),
                    )
                    .child(div().text_size(px(12.0)).child(message))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                Button::new("confirm-cancel")
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, _cx| {
                                        this.confirmation = None;
                                    })),
                            )
                            .child(
                                Button::new("confirm-accept")
                                    .label(accept)
                                    .when(dangerous, |button| button.danger())
                                    .when(!dangerous, |button| button.primary())
                                    .on_click(cx.listener(|this, action, _, cx| {
                                        let _ = action;
                                        let pending = this.confirmation.take();
                                        if let Some(pending) = pending {
                                            if this.accept_confirmation(pending) {
                                                cx.quit();
                                            }
                                        }
                                    })),
                            ),
                    ),
            )
    }
}

fn format_speed_label(speed: f64) -> String {
    if speed <= 0.0 {
        "—".to_string()
    } else {
        format::format_speed(Some(speed))
    }
}

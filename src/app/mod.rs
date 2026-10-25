//! Desktop UI layer built on egui/eframe.
//!
//! The UI is deliberately split from the download engine: every side effect
//! goes through [`backend::AppBackend`], and the UI only renders state and
//! sends commands. This keeps the rendering path free of I/O and makes the
//! data layer testable without a window.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::{Color32, CornerRadius, Rect, RichText, Ui, UiBuilder};

use crate::app::backend::{AppBackend, Command, QueueAction, Startup, UiEvent};
use crate::app::state::{
    AddDownloadsState, Confirmation, DashboardState, DiscoveryState, HistoryState, Page,
    QueueState, SettingsState, Severity,
};
use crate::config::Settings;
use crate::models::download_status::DownloadState;

pub mod add_downloads;
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

/// Left navigation width.
const NAV_WIDTH: f32 = 196.0;
/// Bottom status bar height.
const STATUS_HEIGHT: f32 = 30.0;

/// The root application: owns the UI state and talks to the backend actor.
pub struct BulkDownloadApp {
    pub(crate) backend: AppBackend,
    pub(crate) events: Receiver<UiEvent>,
    pub(crate) settings: Settings,
    pub(crate) page: Page,
    pub(crate) queue: QueueState,
    pub(crate) history: HistoryState,
    pub(crate) discovery: DiscoveryState,
    pub(crate) add: AddDownloadsState,
    pub(crate) settings_state: SettingsState,
    pub(crate) dashboard: DashboardState,
    pub(crate) notifications: state::Notifications,
    pub(crate) confirmation: Option<Confirmation>,
    pub(crate) settings_error: Option<String>,
    /// Folder picked on the Discover page; falls back to the configured one.
    pub(crate) discovery_destination: Option<std::path::PathBuf>,
    /// Line count of the URL box on the previous frame; used to clear the
    /// single-URL overrides when the batch size changes.
    pub(crate) add_line_count: usize,
    pub(crate) history_dirty: bool,
    pub(crate) dashboard_dirty: bool,
    /// Set once the user has accepted the quit-with-active confirmation.
    pub(crate) quit_requested: bool,
    /// Guards against prompting twice for recovered downloads.
    pub(crate) recovered_prompted: bool,
    /// Timestamp of the previous frame, for toast countdowns.
    last_tick: std::time::Instant,
}

impl BulkDownloadApp {
    /// Build the app around an already-started backend.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        backend: AppBackend,
        startup: Startup,
        events: Receiver<UiEvent>,
    ) -> Self {
        theme::apply(&cc.egui_ctx);

        let mut settings_state = SettingsState::default();
        settings_state.load(startup.settings.clone());

        let recovered = startup.recovered.len();
        let mut confirmation = None;
        if recovered > 0 {
            confirmation = Some(Confirmation::ResumeInterrupted { count: recovered });
        }

        let app = Self {
            backend,
            events,
            settings: startup.settings,
            page: Page::Dashboard,
            queue: QueueState::default(),
            history: HistoryState::default(),
            discovery: DiscoveryState::default(),
            add: AddDownloadsState::new(),
            settings_state,
            dashboard: DashboardState::default(),
            notifications: state::Notifications::default(),
            confirmation,
            settings_error: startup.settings_error,
            discovery_destination: None,
            add_line_count: 0,
            history_dirty: true,
            dashboard_dirty: true,
            quit_requested: false,
            recovered_prompted: recovered > 0,
            last_tick: std::time::Instant::now(),
        };

        app.backend.send(Command::LoadQueue);
        app.backend.send(Command::RefreshDashboard);
        app
    }

    // ---- helpers used by the pages ---------------------------------------

    pub(crate) fn send(&self, command: Command) {
        self.backend.send(command);
    }

    pub(crate) fn toast(&mut self, severity: Severity, text: impl Into<String>) {
        self.notifications.push(severity, text);
    }

    /// Ask the backend for the current history page.
    pub(crate) fn request_history(&mut self) {
        self.history.loading = true;
        self.history_dirty = false;
        self.backend.send(Command::LoadHistory {
            query: self.history.to_query(),
        });
    }

    pub(crate) fn request_dashboard(&mut self) {
        self.dashboard_dirty = false;
        self.backend.send(Command::RefreshDashboard);
    }

    pub(crate) fn confirm(&mut self, confirmation: Confirmation) {
        self.confirmation = Some(confirmation);
    }

    fn last_tick(&self) -> std::time::Instant {
        self.last_tick
    }

    fn mark_tick(&mut self, now: std::time::Instant) {
        self.last_tick = now;
    }

    /// Number of rows currently holding a worker slot.
    pub(crate) fn active_count(&self) -> usize {
        self.queue
            .records()
            .iter()
            .filter(|record| state::is_active(record.state))
            .count()
    }

    /// Aggregate throughput of the live rows, in bytes per second.
    pub(crate) fn aggregate_speed(&self) -> f64 {
        self.queue
            .records()
            .iter()
            .map(|record| self.queue.view_for(record).speed_bps)
            .sum()
    }

    // ---- event pump -------------------------------------------------------

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            self.apply_event(event, ctx);
        }
    }

    fn apply_event(&mut self, event: UiEvent, ctx: &egui::Context) {
        match event {
            UiEvent::Progress(progress) => {
                // The engine snapshot and the UI model are deliberately
                // separate types; mirror the fields across the boundary.
                self.queue
                    .apply_progress(crate::models::download_status::DownloadProgress {
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
                }
                if self.page == Page::Dashboard {
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
            UiEvent::AddPreview(parsed) => self.add.preview = Some(*parsed),
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
                    apply_theme(ctx, self.settings.theme);
                    self.toast(Severity::Success, "Settings saved.");
                }
                Err(message) => {
                    self.settings_state.set_error(Some(message.clone()));
                    self.toast(Severity::Error, message);
                }
            },
            UiEvent::HistoryCleared { removed, error } => {
                match error {
                    Some(message) => self.toast(Severity::Error, message),
                    None => self.toast(
                        if removed == 0 {
                            Severity::Info
                        } else {
                            Severity::Success
                        },
                        format!("Cleared {removed} history record(s)."),
                    ),
                }
                self.history_dirty = true;
                self.dashboard_dirty = true;
            }
            UiEvent::Error(message) => {
                self.add.error = Some(message.clone());
                self.toast(Severity::Error, message);
            }
        }
        ctx.request_repaint();
    }

    // ---- shell ------------------------------------------------------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (switch, quit) = ctx.input(|input| {
            let command = input.modifiers.command;
            let switch = Page::ALL
                .iter()
                .enumerate()
                .find(|(index, _)| {
                    command
                        && input.key_pressed(match index {
                            0 => egui::Key::Num1,
                            1 => egui::Key::Num2,
                            2 => egui::Key::Num3,
                            3 => egui::Key::Num4,
                            4 => egui::Key::Num5,
                            _ => egui::Key::Num6,
                        })
                })
                .map(|(_, page)| *page);
            (switch, command && input.key_pressed(egui::Key::Q))
        });

        if let Some(page) = switch {
            self.switch_page(page);
        }
        if quit {
            self.request_quit(ctx);
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) && self.confirmation.is_some() {
            self.confirmation = None;
        }
    }

    fn switch_page(&mut self, page: Page) {
        if self.page == page {
            return;
        }
        self.page = page;
        match page {
            Page::History => self.history_dirty = true,
            Page::Dashboard => self.dashboard_dirty = true,
            Page::Queue => self.backend.send(Command::LoadQueue),
            _ => {}
        }
    }

    fn request_quit(&mut self, ctx: &egui::Context) {
        let active = self.active_count();
        if active > 0 && !self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm(Confirmation::QuitWithActive { count: active });
        } else {
            self.quit_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn nav(&mut self, ui: &mut Ui) {
        ui.add_space(14.0);
        ui.label(
            RichText::new("Bulk Download")
                .size(17.0)
                .strong()
                .color(theme::TEXT),
        );
        ui.label(
            RichText::new("Manager")
                .size(17.0)
                .strong()
                .color(theme::ACCENT),
        );
        ui.add_space(16.0);

        for page in Page::ALL {
            let active = page == self.page;
            let fill = if active { theme::SURFACE } else { theme::BG };
            let text_color = if active { theme::TEXT } else { theme::MUTED };
            let response = ui.add_sized(
                [NAV_WIDTH - 28.0, 34.0],
                egui::Button::new(RichText::new(page.title()).color(text_color))
                    .fill(fill)
                    .stroke(if active {
                        egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6))
                    } else {
                        egui::Stroke::NONE
                    }),
            );
            let response = response.on_hover_text(page.shortcut());
            if response.clicked() {
                self.switch_page(page);
            }
            ui.add_space(2.0);
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(12.0);
        let active = self.active_count();
        ui.label(
            RichText::new(format!("Active: {active}"))
                .color(theme::MUTED)
                .size(12.0),
        );
        ui.label(
            RichText::new(format!("Queued: {}", self.queue.len()))
                .color(theme::MUTED)
                .size(12.0),
        );
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let speed = self.aggregate_speed();
        ui.horizontal_centered(|ui| {
            ui.label(
                RichText::new(format!("{} — {}", self.page.title(), self.page.shortcut()))
                    .color(theme::MUTED)
                    .size(12.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} active · {}",
                        self.active_count(),
                        format::format_speed(Some(speed))
                    ))
                    .color(if self.active_count() > 0 {
                        theme::ACCENT
                    } else {
                        theme::MUTED
                    })
                    .size(12.0),
                );
                ui.label(
                    RichText::new(format!("Queue {}", self.queue.len()))
                        .color(theme::MUTED)
                        .size(12.0),
                );
            });
        });
    }

    fn render_page(&mut self, ui: &mut Ui) {
        match self.page {
            Page::Dashboard => dashboard::show(self, ui),
            Page::AddDownloads => add_downloads::show(self, ui),
            Page::Discover => discover::show(self, ui),
            Page::Queue => queue_view::show(self, ui),
            Page::History => history_view::show(self, ui),
            Page::Settings => settings_view::show(self, ui),
        }
    }

    fn render_confirmation(&mut self, ui: &mut Ui, full: Rect) {
        let Some(confirmation) = self.confirmation.clone() else {
            return;
        };

        ui.painter()
            .rect_filled(full, CornerRadius::ZERO, Color32::from_black_alpha(160));

        let size = egui::vec2(460.0, 0.0);
        let rect = Rect::from_center_size(full.center(), egui::vec2(size.x, 190.0));
        let mut accept = false;
        let mut cancel = false;

        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            components::card().show(ui, |ui| {
                ui.set_width(size.x - 28.0);
                ui.label(
                    RichText::new(confirmation.title())
                        .size(17.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(confirmation.message()).color(theme::MUTED));
                ui.add_space(10.0);

                if confirmation == Confirmation::ClearHistory {
                    ui.checkbox(
                        &mut self.history.keep_files_on_clear,
                        "Also delete downloaded files from disk",
                    );
                    ui.add_space(6.0);
                }

                ui.horizontal(|ui| {
                    if confirmation.is_destructive() {
                        accept =
                            components::danger_button(ui, confirmation.accept_label()).clicked();
                    } else {
                        accept =
                            components::primary_button(ui, confirmation.accept_label()).clicked();
                    }
                    cancel = components::secondary_button(ui, "Cancel").clicked();
                });
            });
        });

        if accept {
            self.accept_confirmation(confirmation, ui.ctx());
        } else if cancel {
            self.confirmation = None;
        }
    }

    fn accept_confirmation(&mut self, confirmation: Confirmation, ctx: &egui::Context) {
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
                    // The queue may not have loaded yet; retry the whole queue.
                    self.backend.send(Command::StartAll);
                } else {
                    self.backend.send(Command::QueueAction {
                        ids,
                        action: QueueAction::Resume,
                    });
                }
            }
            Confirmation::ClearHistory => {
                // The checkbox says "also delete files", the repository takes
                // the opposite flag.
                let keep_files = !self.history.keep_files_on_clear;
                self.backend.send(Command::ClearHistory { keep_files });
            }
            Confirmation::QuitWithActive { .. } => {
                self.quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Confirmation::RemoveQueue { ids } => {
                self.backend.send(Command::QueueAction {
                    ids,
                    action: QueueAction::Remove,
                });
            }
            Confirmation::CancelQueue { ids } => {
                self.backend.send(Command::QueueAction {
                    ids,
                    action: QueueAction::Cancel,
                });
            }
        }
    }

    fn toasts(&mut self, ui: &mut Ui, full: Rect) {
        if self.notifications.is_empty() {
            return;
        }
        let width = 360.0;
        let rect = Rect::from_min_size(
            egui::pos2(
                full.right() - width - 16.0,
                full.bottom() - STATUS_HEIGHT - 16.0 - 60.0,
            ),
            egui::vec2(width, 60.0),
        );
        let mut offset = 0.0;
        let items: Vec<_> = self.notifications.items().to_vec();
        for notification in &items {
            let item_rect = Rect::from_min_size(
                egui::pos2(rect.left(), rect.top() - offset),
                egui::vec2(width, 0.0),
            );
            ui.scope_builder(UiBuilder::new().max_rect(item_rect), |ui| {
                components::toast(ui, notification);
            });
            offset += 46.0;
        }
    }

    fn schedule_repaint(&self, ctx: &egui::Context) {
        let busy = self.queue.has_active() || self.discovery.scanning || self.history.loading;
        let delay = if busy {
            Duration::from_millis(250)
        } else {
            match self.page {
                Page::Queue => Duration::from_millis(500),
                Page::Dashboard | Page::History => Duration::from_millis(1500),
                Page::AddDownloads | Page::Discover | Page::Settings => return,
            }
        };
        ctx.request_repaint_after(delay);
    }
}

/// Apply the stored theme preference. Only Dark is fully styled; the others
/// fall back to egui's stock visuals (documented parity gap with Python).
pub(crate) fn apply_theme(ctx: &egui::Context, theme_preference: crate::config::Theme) {
    match theme_preference {
        crate::config::Theme::Dark => theme::apply(ctx),
        crate::config::Theme::Light => ctx.set_visuals(egui::Visuals::light()),
        crate::config::Theme::System => theme::apply(ctx),
    }
}

impl eframe::App for BulkDownloadApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if ctx.input(|input| input.viewport().close_requested()) && !self.quit_requested {
            if self.active_count() > 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm(Confirmation::QuitWithActive {
                    count: self.active_count(),
                });
            } else {
                self.quit_requested = true;
            }
        }

        self.drain_events(&ctx);
        self.handle_shortcuts(&ctx);

        if self.history_dirty && self.page == Page::History {
            self.request_history();
        }
        if self.dashboard_dirty && self.page == Page::Dashboard {
            self.request_dashboard();
        }

        let now = std::time::Instant::now();
        let delta = now.duration_since(self.last_tick());
        self.notifications
            .tick(delta.min(Duration::from_millis(500)));

        let full = ui.max_rect();
        ui.painter()
            .rect_filled(full, CornerRadius::ZERO, theme::BG);

        let nav_rect = Rect::from_min_size(full.min, egui::vec2(NAV_WIDTH, full.height()));
        let page_rect = Rect::from_min_size(
            egui::pos2(full.left() + NAV_WIDTH, full.top()),
            egui::vec2(full.width() - NAV_WIDTH, full.height() - STATUS_HEIGHT),
        );
        let status_rect = Rect::from_min_size(
            egui::pos2(full.left(), full.bottom() - STATUS_HEIGHT),
            egui::vec2(full.width(), STATUS_HEIGHT),
        );

        ui.scope_builder(UiBuilder::new().max_rect(nav_rect), |ui| {
            self.nav(ui);
        });

        ui.scope_builder(UiBuilder::new().max_rect(page_rect), |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| self.render_page(ui));
        });

        ui.scope_builder(UiBuilder::new().max_rect(status_rect), |ui| {
            ui.separator();
            self.status_bar(ui);
        });

        self.toasts(ui, full);
        self.render_confirmation(ui, page_rect);
        self.mark_tick(now);
        self.schedule_repaint(&ctx);
    }
}

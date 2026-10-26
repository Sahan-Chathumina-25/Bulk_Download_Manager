//! Desktop UI layer built on egui/eframe.
//!
//! The UI is deliberately split from the download engine: every side effect
//! goes through [`backend::AppBackend`], and the UI only renders state and
//! sends commands. This keeps the rendering path free of I/O and makes the
//! data layer testable without a window.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use egui::{
    Align2, Color32, CornerRadius, FontId, Margin, Rect, Response, RichText, Sense, Stroke, Ui,
    UiBuilder, pos2, vec2,
};

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
const NAV_WIDTH: f32 = 220.0;
/// Bottom status bar height.
const STATUS_HEIGHT: f32 = 30.0;
/// Maximum width of the page content column; wider windows centre it.
const CONTENT_MAX_WIDTH: f32 = 1180.0;

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
    /// False once the backend event channel disconnects. The sidebar only
    /// reports connectivity it can actually observe.
    events_connected: bool,
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
            events_connected: true,
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

    /// Navigate to another page from a page-level action.
    pub(crate) fn navigate_to(&mut self, page: Page) {
        self.switch_page(page);
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

    // ---- event pump -------------------------------------------------------

    fn drain_events(&mut self, ctx: &egui::Context) {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.apply_event(event, ctx),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.events_connected = false;
                    break;
                }
            }
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
                self.add.preview_pending = false;
                self.add.add_pending = false;
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
        let palette = theme::palette(ui);
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, palette.sidebar);

        ui.add_space(theme::SPACE_5);
        ui.horizontal(|ui| {
            ui.add_space(theme::SPACE_4);
            let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            paint_logo(ui.painter(), rect, palette);
            ui.add_space(theme::SPACE_2);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("BulkDownloadManager")
                        .size(13.5)
                        .strong()
                        .color(palette.text),
                );
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(theme::SMALL_SIZE)
                        .color(palette.muted),
                );
            });
        });
        ui.add_space(theme::SPACE_5);
        separator(ui, palette);
        ui.add_space(theme::SPACE_3);

        let queued = self.queue.len();
        let queue_badge = (queued > 0).then(|| {
            if queued > 99 {
                "99+".to_string()
            } else {
                queued.to_string()
            }
        });

        for page in Page::ALL {
            let badge = if page == Page::Queue {
                queue_badge.as_deref()
            } else {
                None
            };
            let response = nav_item(ui, page, page == self.page, badge, palette);
            if response.clicked() {
                self.navigate_to(page);
            }
        }

        let active = self.active_count();
        let connected = self.events_connected;
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(theme::SPACE_3);
            ui.horizontal(|ui| {
                ui.add_space(theme::SPACE_4);
                let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                let color = if connected {
                    palette.success
                } else {
                    palette.error
                };
                ui.painter().circle_filled(rect.center(), 4.0, color);
                ui.label(
                    RichText::new(if connected {
                        "Engine connected"
                    } else {
                        "Engine offline"
                    })
                    .size(theme::SMALL_SIZE)
                    .color(palette.text_secondary),
                );
            });
            ui.horizontal(|ui| {
                ui.add_space(theme::SPACE_4);
                ui.label(
                    RichText::new(format!("{active} active · {queued} queued"))
                        .size(theme::SMALL_SIZE)
                        .color(palette.muted),
                );
            });
            ui.add_space(theme::SPACE_2);
            separator(ui, palette);
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let palette = theme::palette(ui);
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, palette.sidebar);
        let active = self.active_count();
        let queued = self.queue.len();
        let state = if active > 0 {
            "Downloading"
        } else if queued > 0 {
            "Queued"
        } else {
            "Idle"
        };

        ui.horizontal_centered(|ui| {
            ui.label(
                RichText::new(format!("{} — {}", self.page.title(), state))
                    .size(theme::SMALL_SIZE)
                    .color(palette.muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(theme::SMALL_SIZE)
                        .color(palette.muted),
                );
                ui.label(
                    RichText::new(format!("{queued} queued"))
                        .size(theme::SMALL_SIZE)
                        .color(palette.muted),
                );
                ui.label(
                    RichText::new(format!("{active} active"))
                        .size(theme::SMALL_SIZE)
                        .color(if active > 0 {
                            palette.accent
                        } else {
                            palette.muted
                        }),
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

        let width = 460.0;
        let rect = Rect::from_center_size(full.center(), vec2(width, 230.0));
        let mut result = components::DialogResult::default();

        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            result = components::confirmation_dialog(
                ui,
                width,
                confirmation.title(),
                &confirmation.message(),
                confirmation.accept_label(),
                confirmation.is_destructive(),
                |ui| {
                    if confirmation == Confirmation::ClearHistory {
                        ui.checkbox(
                            &mut self.history.keep_files_on_clear,
                            "Also delete downloaded files from disk",
                        );
                    }
                },
            );
        });

        if result.accepted {
            self.accept_confirmation(confirmation, ui.ctx());
        } else if result.cancelled {
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
        let width = 380.0;
        // Reserve the strip above the status bar and anchor the stack to its
        // bottom edge, so a wrapped (taller) toast grows upward and is never
        // clipped at the window edge or painted over the status bar.
        let area = Rect::from_min_max(
            pos2(
                full.right() - width - theme::SPACE_4,
                full.top() + theme::SPACE_4,
            ),
            pos2(
                full.right() - theme::SPACE_4,
                full.bottom() - STATUS_HEIGHT - theme::SPACE_4,
            ),
        );
        let items = self.notifications.items();
        ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
            ui.set_clip_rect(area);
            ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                for notification in items.iter().rev() {
                    components::toast(ui, notification);
                    ui.add_space(theme::SPACE_1);
                }
            });
        });
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

/// A one-pixel hairline across the current UI.
fn separator(ui: &mut Ui, palette: theme::Palette) {
    let rect = ui.available_rect_before_wrap();
    let y = rect.top() + 0.5;
    ui.painter().line_segment(
        [pos2(rect.left(), y), pos2(rect.right(), y)],
        Stroke::new(1.0, palette.border),
    );
    ui.allocate_exact_size(vec2(rect.width(), 1.0), Sense::hover());
}

/// One sidebar navigation row: painted icon, label, hover and selected state.
fn nav_item(
    ui: &mut Ui,
    page: Page,
    selected: bool,
    badge: Option<&str>,
    palette: theme::Palette,
) -> Response {
    let height = 38.0;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let item = rect.shrink2(vec2(theme::SPACE_2, 2.0));

    let fill = if selected {
        palette.accent.gamma_multiply(0.16)
    } else if response.hovered() {
        palette.hover
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(item, CornerRadius::same(theme::RADIUS_INPUT), fill);

    if selected {
        let indicator =
            Rect::from_min_size(pos2(item.left(), item.center().y - 9.0), vec2(3.0, 18.0));
        painter.rect_filled(indicator, CornerRadius::same(2), palette.accent);
    }

    let icon_color = if selected || response.hovered() {
        palette.text
    } else {
        palette.text_secondary
    };
    let icon_rect =
        Rect::from_center_size(pos2(item.left() + 20.0, item.center().y), vec2(16.0, 16.0));
    paint_page_icon(painter, icon_rect, page, icon_color);

    let label_color = if selected {
        palette.text
    } else {
        palette.text_secondary
    };
    painter.text(
        pos2(item.left() + 38.0, item.center().y),
        Align2::LEFT_CENTER,
        page.title(),
        FontId::proportional(theme::BODY_SIZE),
        label_color,
    );

    if let Some(badge) = badge {
        let pill_width = if badge.len() > 2 { 34.0 } else { 22.0 };
        let pill = Rect::from_center_size(
            pos2(item.right() - 20.0, item.center().y),
            vec2(pill_width, 18.0),
        );
        painter.rect_filled(pill, CornerRadius::same(9), palette.accent);
        painter.text(
            pill.center(),
            Align2::CENTER_CENTER,
            badge,
            FontId::proportional(10.0),
            Color32::WHITE,
        );
    }

    response.on_hover_text(page.shortcut())
}

/// Geometric logo mark: a folder/tray with a download arrow.
fn paint_logo(painter: &egui::Painter, rect: Rect, palette: theme::Palette) {
    let mark = rect.shrink(1.0);
    painter.rect_filled(
        mark,
        CornerRadius::same(theme::RADIUS_INPUT),
        palette.accent.gamma_multiply(0.18),
    );

    let stroke = Stroke::new(2.0, palette.accent_hover);
    let center = mark.center();
    painter.line_segment(
        [
            pos2(center.x, mark.top() + 7.0),
            pos2(center.x, mark.bottom() - 13.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            pos2(center.x - 4.5, mark.bottom() - 17.5),
            pos2(center.x, mark.bottom() - 13.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            pos2(center.x + 4.5, mark.bottom() - 17.5),
            pos2(center.x, mark.bottom() - 13.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            pos2(mark.left() + 7.0, mark.bottom() - 7.0),
            pos2(mark.right() - 7.0, mark.bottom() - 7.0),
        ],
        stroke,
    );
}

/// Consistent painted vector glyphs for the navigation rail.
fn paint_page_icon(painter: &egui::Painter, rect: Rect, page: Page, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let center = rect.center();

    match page {
        Page::Dashboard => {
            let size = 6.0;
            let step = 5.0;
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let square = Rect::from_center_size(
                    pos2(center.x + dx * step, center.y + dy * step),
                    vec2(size, size),
                );
                painter.rect_filled(square, CornerRadius::same(1), color);
            }
        }
        Page::AddDownloads => {
            painter.line_segment(
                [
                    pos2(center.x - 5.5, center.y),
                    pos2(center.x + 5.5, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(center.x, center.y - 5.5),
                    pos2(center.x, center.y + 5.5),
                ],
                stroke,
            );
        }
        Page::Discover => {
            painter.circle_stroke(pos2(center.x - 1.5, center.y - 1.5), 4.5, stroke);
            painter.line_segment(
                [
                    pos2(center.x + 1.8, center.y + 1.8),
                    pos2(center.x + 6.0, center.y + 6.0),
                ],
                Stroke::new(1.8, color),
            );
        }
        Page::Queue => {
            for offset in [-4.5, 0.0, 4.5] {
                painter.circle_filled(pos2(center.x - 5.5, center.y + offset), 1.3, color);
                painter.line_segment(
                    [
                        pos2(center.x - 2.0, center.y + offset),
                        pos2(center.x + 6.0, center.y + offset),
                    ],
                    stroke,
                );
            }
        }
        Page::History => {
            painter.circle_stroke(center, 5.5, stroke);
            painter.line_segment([center, pos2(center.x, center.y - 3.0)], stroke);
            painter.line_segment([center, pos2(center.x + 2.6, center.y + 1.0)], stroke);
        }
        Page::Settings => {
            for (offset, knob) in [(-4.0, 2.5), (4.0, -2.0)] {
                let y = center.y + offset;
                painter.line_segment([pos2(center.x - 6.0, y), pos2(center.x + 6.0, y)], stroke);
                painter.circle_filled(pos2(center.x + knob, y), 2.0, color);
            }
        }
    }
}

/// Apply the stored theme preference.
///
/// Every variant installs the Midnight Indigo style pair first, so switching
/// themes only changes which of the two palettes is active and never resets
/// any widget, settings or download state.
pub(crate) fn apply_theme(ctx: &egui::Context, theme_preference: crate::config::Theme) {
    match theme_preference {
        crate::config::Theme::Dark => theme::apply_dark(ctx),
        crate::config::Theme::Light => theme::apply_light(ctx),
        crate::config::Theme::System => theme::apply_system(ctx),
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

        // Anchor the shell to the real viewport rect: the root `Ui` can be
        // inset from the framebuffer, which used to leave an unpainted band
        // below the status strip.
        let full = ui.ctx().content_rect();
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
            egui::Frame::new()
                .inner_margin(Margin::symmetric(
                    theme::SPACE_6 as i8,
                    theme::SPACE_5 as i8,
                ))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            // Never let the content column claim more width
                            // than the panel actually has (sidebar included).
                            let available = ui.available_width();
                            let content_width = available.min(CONTENT_MAX_WIDTH);
                            let padding = ((available - content_width) / 2.0).max(0.0);
                            ui.horizontal(|ui| {
                                ui.add_space(padding);
                                ui.vertical(|ui| {
                                    ui.set_width(content_width);
                                    ui.set_max_width(content_width);
                                    self.render_page(ui);
                                    ui.add_space(theme::SPACE_6);
                                });
                            });
                        });
                });
        });

        ui.scope_builder(UiBuilder::new().max_rect(status_rect), |ui| {
            self.status_bar(ui);
        });

        self.toasts(ui, full);
        self.render_confirmation(ui, page_rect);
        self.mark_tick(now);
        self.schedule_repaint(&ctx);
    }
}

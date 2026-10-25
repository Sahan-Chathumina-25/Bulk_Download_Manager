//! Colour palette and global egui styling.
//!
//! Hex values are the ones the Python application used (see
//! `legacy-python/src/app/theme.py`), converted to `Color32` at compile time.

use egui::{Color32, Context, CornerRadius, Stroke, Visuals};

use crate::app::state::Severity;
use crate::models::download_status::DownloadState;

/// Window and panel background.
pub const BG: Color32 = Color32::from_rgb(0x1e, 0x1f, 0x24);
/// Card background.
pub const SURFACE: Color32 = Color32::from_rgb(0x26, 0x27, 0x2e);
/// Slightly raised surface (inputs, alternate rows).
pub const SURFACE_ALT: Color32 = Color32::from_rgb(0x2e, 0x2f, 0x38);
/// Hairline borders and separators.
pub const BORDER: Color32 = Color32::from_rgb(0x3a, 0x3c, 0x46);
/// Primary accent (buttons, selection, focus).
pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x7c, 0xff);
/// Accent under the cursor.
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x6b, 0x93, 0xff);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xee);
/// Secondary text, labels, hints.
pub const MUTED: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xae);
/// Completed / success.
pub const SUCCESS: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x6d);
/// Paused / warning.
pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xa5, 0x3c);
/// Failed / destructive.
pub const ERROR: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);

/// Colour of the badge and progress bar for a download state.
pub fn state_color(state: DownloadState) -> Color32 {
    match state {
        DownloadState::Queued => MUTED,
        DownloadState::Connecting | DownloadState::Downloading => ACCENT,
        DownloadState::Paused => WARN,
        DownloadState::Completed => SUCCESS,
        DownloadState::Failed => ERROR,
        DownloadState::Cancelled => MUTED,
        DownloadState::Interrupted => WARN,
    }
}

/// Capitalised label for a download state.
pub fn state_label(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => "Queued",
        DownloadState::Connecting => "Connecting",
        DownloadState::Downloading => "Downloading",
        DownloadState::Paused => "Paused",
        DownloadState::Completed => "Completed",
        DownloadState::Failed => "Failed",
        DownloadState::Cancelled => "Cancelled",
        DownloadState::Interrupted => "Interrupted",
    }
}

/// Colour for a toast of the given severity.
pub fn severity_color(severity: Severity) -> Color32 {
    match severity {
        Severity::Info => ACCENT,
        Severity::Success => SUCCESS,
        Severity::Warning => WARN,
        Severity::Error => ERROR,
    }
}

/// Install the dark palette on a fresh context.
pub fn apply(ctx: &Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = SURFACE_ALT;
    visuals.faint_bg_color = SURFACE_ALT;
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT_HOVER;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = ERROR;
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0, TEXT);

    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.weak_bg_fill = SURFACE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

    visuals.widgets.inactive.bg_fill = SURFACE_ALT;
    visuals.widgets.inactive.weak_bg_fill = SURFACE_ALT;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);

    visuals.widgets.hovered.bg_fill = SURFACE;
    visuals.widgets.hovered.weak_bg_fill = SURFACE;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_HOVER);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);

    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.weak_bg_fill = ACCENT;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_HOVER);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);

    visuals.widgets.open.bg_fill = SURFACE_ALT;
    visuals.widgets.open.weak_bg_fill = SURFACE_ALT;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);

    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(6);
    }

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size.y = 26.0;
    ctx.set_style_of(egui::Theme::Dark, style);
}

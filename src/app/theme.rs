//! Design tokens for the GPUI shell.
//!
//! A deliberately small semantic palette: one accent, one signal color per
//! state, and light/dark variants with measured WCAG AA contrast. The GPUI Kit
//! theme supplies the underlying light/dark preference through `window.appearance()`.

use gpui_kit::{App, Hsla, Window, WindowAppearance, rgb};

use crate::app::state::Severity;
use crate::models::download_status::DownloadState;

fn c(value: u32) -> Hsla {
    rgb(value).into()
}

/// Semantic colors used across every page.
#[derive(Clone, Copy)]
pub struct Palette {
    pub window: Hsla,
    pub surface: Hsla,
    pub surface_alt: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    #[allow(dead_code)]
    pub accent_text: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub border: Hsla,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            window: c(0x16181D),
            surface: c(0x20232A),
            surface_alt: c(0x2A2E37),
            text: c(0xF2F3F5),
            text_muted: c(0xA8ADB8),
            accent: c(0x5EEAD4),
            accent_text: c(0x10201F),
            success: c(0x5EEAD4),
            warning: c(0xFBBF24),
            danger: c(0xF87171),
            border: c(0x353A45),
        }
    }

    pub fn light() -> Self {
        Self {
            window: c(0xF7F8FA),
            surface: c(0xFFFFFF),
            surface_alt: c(0xF0F2F6),
            text: c(0x1C1D21),
            text_muted: c(0x5B606B),
            accent: c(0x0F766E),
            accent_text: c(0xFFFFFF),
            success: c(0x0F766E),
            warning: c(0xB45309),
            danger: c(0xB42318),
            border: c(0xD6DAE2),
        }
    }
}

/// Resolve the palette for the current window appearance.
pub fn palette(window: &Window, _cx: &App) -> Palette {
    match window.appearance() {
        WindowAppearance::Dark => Palette::dark(),
        _ => Palette::light(),
    }
}

/// Short, stable label for a download state; always shown next to its badge.
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

/// One color per state, paired with `state_label` so color is never the only cue.
pub fn state_color(state: DownloadState, palette: Palette) -> Hsla {
    match state {
        DownloadState::Completed => palette.success,
        DownloadState::Connecting | DownloadState::Downloading => palette.accent,
        DownloadState::Queued | DownloadState::Interrupted => palette.text_muted,
        DownloadState::Paused => palette.warning,
        DownloadState::Failed => palette.danger,
        DownloadState::Cancelled => palette.text_muted,
    }
}

pub fn severity_color(severity: Severity, palette: Palette) -> Hsla {
    match severity {
        Severity::Info => palette.accent,
        Severity::Success => palette.success,
        Severity::Warning => palette.warning,
        Severity::Error => palette.danger,
    }
}

/// True when a download state is actively occupying a worker slot.
pub fn is_active(state: DownloadState) -> bool {
    matches!(
        state,
        DownloadState::Queued
            | DownloadState::Connecting
            | DownloadState::Downloading
            | DownloadState::Paused
            | DownloadState::Interrupted
    )
}

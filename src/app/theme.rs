//! Centralised design system: colour tokens, typography, spacing and radius,
//! plus the global [`egui::Style`] installation.
//!
//! Stage B of the redesign ("Midnight Indigo"). Pages should stop hard-coding
//! values and pull everything from here. Two palettes exist: the default dark
//! palette exposed as module constants (kept for the existing call sites) and a
//! matching light palette available through [`Palette::LIGHT`]. Components read
//! the active palette from [`palette`] so they follow the theme preference.

use std::collections::BTreeMap;

use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme,
    ThemePreference, Vec2, Visuals,
};

use crate::app::state::Severity;
use crate::models::download_status::DownloadState;

// ---------------------------------------------------------------------------
// Colour tokens — dark (Midnight Indigo)
// ---------------------------------------------------------------------------

/// Application background.
pub const BG: Color32 = Color32::from_rgb(0x0B, 0x10, 0x20);
/// Navigation rail background.
pub const SIDEBAR: Color32 = Color32::from_rgb(0x0F, 0x17, 0x2A);
/// Content surface (cards, panels).
pub const SURFACE: Color32 = Color32::from_rgb(0x11, 0x1B, 0x2E);
/// Elevated surface (inputs, raised rows).
pub const ELEVATED: Color32 = Color32::from_rgb(0x17, 0x23, 0x38);
/// Backwards-compatible alias for [`ELEVATED`].
#[allow(dead_code, reason = "alias kept for the later redesign stages")]
pub const SURFACE_ALT: Color32 = ELEVATED;
/// Hover / pressed surface.
pub const HOVER: Color32 = Color32::from_rgb(0x20, 0x2E, 0x46);
/// Hairline borders and separators.
pub const BORDER: Color32 = Color32::from_rgb(0x28, 0x36, 0x4F);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xF1, 0xF5, 0xF9);
/// Secondary text (supporting copy).
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xA8, 0xB4, 0xC7);
/// Muted text (captions, disabled-adjacent hints).
pub const MUTED: Color32 = Color32::from_rgb(0x73, 0x81, 0x98);
/// Primary accent — indigo.
pub const ACCENT: Color32 = Color32::from_rgb(0x63, 0x66, 0xF1);
/// Accent under the cursor / focus.
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x81, 0x8C, 0xF8);
/// Violet — retry and secondary highlights.
pub const VIOLET: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6);
/// Cyan — discovery and imports.
pub const CYAN: Color32 = Color32::from_rgb(0x22, 0xD3, 0xEE);
/// Neutral blue — open folder and informational diagnostics.
pub const BLUE: Color32 = Color32::from_rgb(0x60, 0xA5, 0xFA);
/// Success / start / completed.
pub const SUCCESS: Color32 = Color32::from_rgb(0x22, 0xC5, 0x5E);
/// Warning / pause / verification.
pub const WARN: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B);
/// Error / cancel / failed / security rejection.
pub const ERROR: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71);
/// Disabled foreground.
pub const DISABLED: Color32 = Color32::from_rgb(0x4A, 0x56, 0x6B);

// ---------------------------------------------------------------------------
// Spacing and radius scales
// ---------------------------------------------------------------------------

/// 4 pt.
pub const SPACE_1: f32 = 4.0;
/// 8 pt.
pub const SPACE_2: f32 = 8.0;
/// 12 pt.
pub const SPACE_3: f32 = 12.0;
/// 16 pt.
pub const SPACE_4: f32 = 16.0;
/// 20 pt.
pub const SPACE_5: f32 = 20.0;
/// 24 pt.
pub const SPACE_6: f32 = 24.0;
/// 28 pt.
pub const SPACE_7: f32 = 28.0;
/// 32 pt.
#[allow(
    dead_code,
    reason = "scale token consumed by the later redesign stages"
)]
pub const SPACE_8: f32 = 32.0;

/// Radius for compact chips/badges.
pub const RADIUS_SMALL: u8 = 6;
/// Radius for inputs and buttons.
pub const RADIUS_INPUT: u8 = 8;
/// Radius for cards and panels.
pub const RADIUS_PANEL: u8 = 12;
/// Radius for dialogs and popups.
pub const RADIUS_DIALOG: u8 = 16;

// ---------------------------------------------------------------------------
// Typography
// ---------------------------------------------------------------------------

/// Page title size (24–26 pt band).
pub const PAGE_TITLE_SIZE: f32 = 25.0;
/// Section heading size (16–18 pt band).
pub const SECTION_SIZE: f32 = 17.0;
/// Body text size (13–15 pt band).
pub const BODY_SIZE: f32 = 14.0;
/// Supporting label size (11–12 pt band).
pub const SMALL_SIZE: f32 = 11.5;
/// Metrics / big numbers inside statistics tiles.
pub const METRIC_SIZE: f32 = 26.0;

// ---------------------------------------------------------------------------
// Semantic actions
// ---------------------------------------------------------------------------

/// Semantic action colour. Mapping is centralised so pages never pick a raw
/// colour for a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "semantic vocabulary; unused variants are adopted as pages are redesigned"
)]
pub enum Action {
    Primary,
    Secondary,
    Add,
    Import,
    Start,
    Pause,
    Cancel,
    Retry,
    Discover,
    OpenFolder,
    Completed,
    Failed,
    Warning,
    Security,
    Info,
    Settings,
    Disabled,
}

impl Action {
    /// Colour for this action in the active dark palette.
    #[allow(dead_code, reason = "used by the button component in later stages")]
    pub const fn color(self) -> Color32 {
        action_color(self)
    }
}

/// Colour for an action in the default dark palette.
#[allow(
    dead_code,
    reason = "convenience wrapper for the later redesign stages"
)]
pub const fn action_color(action: Action) -> Color32 {
    action_color_in(Palette::DARK, action)
}

/// Colour for an action in an explicit palette.
pub const fn action_color_in(palette: Palette, action: Action) -> Color32 {
    match action {
        Action::Primary | Action::Add | Action::Settings => palette.accent,
        Action::Secondary => palette.text_secondary,
        Action::Import | Action::Discover => palette.cyan,
        Action::Start | Action::Completed => palette.success,
        Action::Pause | Action::Warning => palette.warning,
        Action::Cancel | Action::Failed | Action::Security => palette.error,
        Action::Retry => palette.violet,
        Action::OpenFolder | Action::Info => palette.info,
        Action::Disabled => palette.disabled,
    }
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// A complete set of surface and semantic colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color32,
    pub sidebar: Color32,
    pub surface: Color32,
    pub elevated: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub violet: Color32,
    pub cyan: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub info: Color32,
    pub disabled: Color32,
}

impl Palette {
    /// Midnight Indigo dark palette.
    pub const DARK: Self = Self {
        bg: BG,
        sidebar: SIDEBAR,
        surface: SURFACE,
        elevated: ELEVATED,
        hover: HOVER,
        border: BORDER,
        text: TEXT,
        text_secondary: TEXT_SECONDARY,
        muted: MUTED,
        accent: ACCENT,
        accent_hover: ACCENT_HOVER,
        violet: VIOLET,
        cyan: CYAN,
        success: SUCCESS,
        warning: WARN,
        error: ERROR,
        info: BLUE,
        disabled: DISABLED,
    };

    /// Matching light palette.
    pub const LIGHT: Self = Self {
        bg: Color32::from_rgb(0xF5, 0xF7, 0xFC),
        sidebar: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        elevated: Color32::from_rgb(0xEE, 0xF2, 0xFF),
        hover: Color32::from_rgb(0xE2, 0xE8, 0xF0),
        border: Color32::from_rgb(0xE2, 0xE8, 0xF0),
        text: Color32::from_rgb(0x17, 0x20, 0x33),
        text_secondary: Color32::from_rgb(0x52, 0x60, 0x78),
        muted: Color32::from_rgb(0x6B, 0x7A, 0x90),
        accent: Color32::from_rgb(0x4F, 0x46, 0xE5),
        accent_hover: Color32::from_rgb(0x63, 0x66, 0xF1),
        violet: Color32::from_rgb(0x7C, 0x3A, 0xED),
        cyan: Color32::from_rgb(0x02, 0x84, 0xC7),
        success: Color32::from_rgb(0x15, 0x80, 0x3D),
        warning: Color32::from_rgb(0xB4, 0x53, 0x09),
        error: Color32::from_rgb(0xDC, 0x26, 0x26),
        info: Color32::from_rgb(0x25, 0x63, 0xEB),
        disabled: Color32::from_rgb(0x94, 0xA3, 0xB8),
    };

    /// Pick the palette that matches the given visuals.
    pub fn of(visuals: &Visuals) -> Self {
        if visuals.dark_mode {
            Self::DARK
        } else {
            Self::LIGHT
        }
    }

    /// Colour for a semantic action.
    pub const fn action(self, action: Action) -> Color32 {
        action_color_in(self, action)
    }

    /// Colour of a download state.
    pub const fn state_color(self, state: DownloadState) -> Color32 {
        match state {
            DownloadState::Queued => self.muted,
            DownloadState::Connecting | DownloadState::Downloading => self.accent,
            DownloadState::Paused => self.warning,
            DownloadState::Completed => self.success,
            DownloadState::Failed => self.error,
            DownloadState::Cancelled => self.muted,
            DownloadState::Interrupted => self.warning,
        }
    }

    /// Colour of a toast severity.
    pub const fn severity_color(self, severity: Severity) -> Color32 {
        match severity {
            Severity::Info => self.info,
            Severity::Success => self.success,
            Severity::Warning => self.warning,
            Severity::Error => self.error,
        }
    }
}

/// Palette for the active theme.
pub fn palette(ui: &egui::Ui) -> Palette {
    Palette::of(ui.visuals())
}

/// Colour of the badge and progress bar for a download state.
pub fn state_color(state: DownloadState) -> Color32 {
    Palette::DARK.state_color(state)
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

/// Colour for a toast of the given severity (dark palette).
#[allow(
    dead_code,
    reason = "call-site compatibility for the later redesign stages"
)]
pub fn severity_color(severity: Severity) -> Color32 {
    Palette::DARK.severity_color(severity)
}

// ---------------------------------------------------------------------------
// Global style installation
// ---------------------------------------------------------------------------

/// Install the design system and select the dark theme.
pub fn apply(ctx: &Context) {
    install_styles(ctx);
    ctx.options_mut(|options| options.fallback_theme = Theme::Dark);
    ctx.set_theme(ThemePreference::Dark);
}

/// Install the design system and select the dark theme.
pub fn apply_dark(ctx: &Context) {
    install_styles(ctx);
    ctx.options_mut(|options| options.fallback_theme = Theme::Dark);
    ctx.set_theme(ThemePreference::Dark);
}

/// Install the design system and select the light theme.
pub fn apply_light(ctx: &Context) {
    install_styles(ctx);
    ctx.options_mut(|options| options.fallback_theme = Theme::Light);
    ctx.set_theme(ThemePreference::Light);
}

/// Install the design system and follow the operating-system theme.
///
/// egui/eframe report the platform preference; `fallback_theme` covers the
/// platforms where it is unavailable, so the option is honest rather than a
/// disguised dark theme.
pub fn apply_system(ctx: &Context) {
    install_styles(ctx);
    ctx.options_mut(|options| options.fallback_theme = Theme::Dark);
    ctx.set_theme(ThemePreference::System);
}

/// Install both theme variants so switching never loses the custom styling.
fn install_styles(ctx: &Context) {
    let dark = configured_style(ctx, Theme::Dark, Palette::DARK);
    let light = configured_style(ctx, Theme::Light, Palette::LIGHT);
    ctx.set_style_of(Theme::Dark, dark);
    ctx.set_style_of(Theme::Light, light);
}

fn configured_style(ctx: &Context, theme: Theme, palette: Palette) -> Style {
    let mut style = (*ctx.style_of(theme)).clone();
    style.visuals = visuals_for(palette, theme == Theme::Dark);
    style.text_styles = text_styles();
    style.spacing.item_spacing = Vec2::new(SPACE_2, SPACE_2);
    style.spacing.button_padding = Vec2::new(SPACE_3, 6.0);
    style.spacing.interact_size = Vec2::new(SPACE_1, 30.0);
    style.spacing.window_margin = Margin::same(SPACE_4 as i8);
    style.spacing.menu_margin = Margin::same(SPACE_2 as i8);
    style.spacing.indent = SPACE_4;
    // Subtle transitions only; no continuous animation.
    style.animation_time = 0.10;
    style
}

fn text_styles() -> BTreeMap<TextStyle, FontId> {
    let proportional = FontFamily::Proportional;
    let mut styles = BTreeMap::new();
    styles.insert(TextStyle::Heading, FontId::new(20.0, proportional.clone()));
    styles.insert(
        TextStyle::Body,
        FontId::new(BODY_SIZE, proportional.clone()),
    );
    styles.insert(
        TextStyle::Button,
        FontId::new(BODY_SIZE, proportional.clone()),
    );
    styles.insert(TextStyle::Small, FontId::new(SMALL_SIZE, proportional));
    styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    styles
}

fn visuals_for(palette: Palette, dark: bool) -> Visuals {
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.dark_mode = dark;
    visuals.panel_fill = palette.bg;
    visuals.window_fill = palette.surface;
    visuals.extreme_bg_color = palette.elevated;
    visuals.faint_bg_color = palette.elevated;
    visuals.code_bg_color = palette.elevated;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.muted);
    visuals.hyperlink_color = palette.accent_hover;
    visuals.warn_fg_color = palette.warning;
    visuals.error_fg_color = palette.error;
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    visuals.window_corner_radius = CornerRadius::same(RADIUS_DIALOG);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_PANEL);
    visuals.selection.bg_fill = palette.accent.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, palette.text);

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = palette.surface;
    widgets.noninteractive.weak_bg_fill = palette.surface;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS_SMALL);

    widgets.inactive.bg_fill = palette.elevated;
    widgets.inactive.weak_bg_fill = palette.elevated;
    widgets.inactive.bg_stroke = Stroke::new(1.0, palette.border);
    widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.inactive.corner_radius = CornerRadius::same(RADIUS_INPUT);

    widgets.hovered.bg_fill = palette.hover;
    widgets.hovered.weak_bg_fill = palette.hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0, palette.accent.gamma_multiply(0.8));
    widgets.hovered.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.hovered.corner_radius = CornerRadius::same(RADIUS_INPUT);

    widgets.active.bg_fill = palette.accent;
    widgets.active.weak_bg_fill = palette.accent;
    widgets.active.bg_stroke = Stroke::new(1.0, palette.accent_hover);
    widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    widgets.active.corner_radius = CornerRadius::same(RADIUS_INPUT);

    widgets.open.bg_fill = palette.elevated;
    widgets.open.weak_bg_fill = palette.elevated;
    widgets.open.bg_stroke = Stroke::new(1.0, palette.border);
    widgets.open.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.open.corner_radius = CornerRadius::same(RADIUS_INPUT);

    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_keep_text_above_surface_contrast() {
        for palette in [Palette::DARK, Palette::LIGHT] {
            assert_ne!(palette.text, palette.surface);
            assert_ne!(palette.muted, palette.surface);
            assert_ne!(palette.border, palette.surface);
        }
    }

    #[test]
    fn semantic_actions_use_distinct_families() {
        assert_eq!(action_color(Action::Start), SUCCESS);
        assert_eq!(action_color(Action::Pause), WARN);
        assert_eq!(action_color(Action::Cancel), ERROR);
        assert_eq!(action_color(Action::Retry), VIOLET);
        assert_eq!(action_color(Action::Discover), CYAN);
        assert_eq!(action_color(Action::OpenFolder), BLUE);
        assert_eq!(action_color(Action::Primary), ACCENT);
        assert_eq!(action_color(Action::Disabled), DISABLED);
    }

    #[test]
    fn light_palette_is_not_the_dark_palette() {
        assert_ne!(Palette::LIGHT.bg, Palette::DARK.bg);
        assert_ne!(Palette::LIGHT.text, Palette::DARK.text);
        assert_ne!(Palette::LIGHT.accent, Palette::DARK.accent);
    }

    #[test]
    fn spacing_and_radius_follow_the_scale() {
        let spacing = [
            SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_5, SPACE_6, SPACE_7, SPACE_8,
        ];
        assert!(spacing.windows(2).all(|pair| pair[0] < pair[1]));

        let radii = [RADIUS_SMALL, RADIUS_INPUT, RADIUS_PANEL, RADIUS_DIALOG];
        assert!(radii.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

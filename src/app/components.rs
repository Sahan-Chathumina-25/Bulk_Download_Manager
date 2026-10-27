//! Reusable widgets shared by the six pages.
//!
//! Every helper pulls its colours from [`crate::app::theme::Palette`], so the
//! Glass tokens stay the single source of truth. Panels are built through
//! [`glass_panel`], which paints an opaque surface step, a hairline border, a
//! top-edge highlight and a restrained shadow.

use egui::{
    Button, Color32, CornerRadius, Frame, InnerResponse, Margin, ProgressBar, Response, RichText,
    Sense, Shadow, Stroke, TextEdit, Ui, Vec2,
};

use crate::app::state::Notification;
use crate::app::theme::{self, Action, Palette};

// ---------------------------------------------------------------------------
// Glass containers
// ---------------------------------------------------------------------------

/// Depth level of a glass surface.
///
/// The levels mirror the palette surface steps; they are not transparency
/// levels, so text stays readable at every level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassLevel {
    /// Cards sitting directly on the application background.
    Base,
    /// Inset rows and grouped content.
    Secondary,
    /// Floating content: dialogs, toasts, popovers.
    Elevated,
}

impl GlassLevel {
    fn fill(self, palette: Palette) -> Color32 {
        match self {
            Self::Base => palette.surface,
            Self::Secondary => palette.surface_secondary,
            Self::Elevated => palette.elevated,
        }
    }

    fn border(self, palette: Palette) -> Color32 {
        match self {
            Self::Base | Self::Secondary => palette.border,
            Self::Elevated => palette.border_strong,
        }
    }

    fn highlight(self, palette: Palette) -> Color32 {
        match self {
            Self::Base | Self::Secondary => palette.border_strong,
            Self::Elevated => palette.text_secondary.gamma_multiply(0.28),
        }
    }
}

/// A glass surface frame. Build it with [`glass_panel`] and render it with
/// [`GlassPanel::show`].
pub struct GlassPanel {
    palette: Palette,
    level: GlassLevel,
    frame: Frame,
}

impl GlassPanel {
    /// Override the inner padding.
    pub fn inner_margin(mut self, margin: Margin) -> Self {
        self.frame = self.frame.inner_margin(margin);
        self
    }

    /// Override the corner radius.
    pub fn corner_radius(mut self, radius: u8) -> Self {
        self.frame = self.frame.corner_radius(CornerRadius::same(radius));
        self
    }

    /// Override the fill (used by row selection).
    pub fn fill(mut self, fill: Color32) -> Self {
        self.frame = self.frame.fill(fill);
        self
    }

    /// Override the border (used by semantic toasts).
    pub fn stroke(mut self, stroke: Stroke) -> Self {
        self.frame = self.frame.stroke(stroke);
        self
    }

    /// Drop the shadow for content that sits flush with its parent.
    pub fn without_shadow(mut self) -> Self {
        self.frame = self.frame.shadow(Shadow {
            offset: [0, 0],
            blur: 0,
            spread: 0,
            color: Color32::TRANSPARENT,
        });
        self
    }

    /// Render the panel and paint the top-edge highlight above its fill.
    pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
        let Self {
            palette,
            level,
            frame,
        } = self;
        let response = frame.show(ui, add_contents);
        let rect = response.response.rect;
        // Keep the highlight clear of the rounded corners.
        let inset = (theme::RADIUS_PANEL as f32).min(rect.width() * 0.5);
        ui.painter().hline(
            rect.x_range().shrink(inset),
            rect.top() + 1.0,
            Stroke::new(1.0, level.highlight(palette)),
        );
        response
    }
}

/// Build a glass panel for the active palette.
///
/// `level` selects the surface step; the returned panel carries the shared
/// border, top-edge highlight and shadow treatment.
pub fn glass_panel(ui: &Ui, level: GlassLevel) -> GlassPanel {
    let palette = theme::palette(ui);
    let frame = Frame::new()
        .fill(level.fill(palette))
        .stroke(Stroke::new(1.0, level.border(palette)))
        .shadow(Shadow {
            offset: [0, 2],
            blur: 10,
            spread: 0,
            color: palette.glass_shadow,
        })
        .inner_margin(Margin::same(theme::SPACE_4 as i8))
        .corner_radius(CornerRadius::same(theme::RADIUS_PANEL));
    GlassPanel {
        palette,
        level,
        frame,
    }
}

/// A card that follows the active palette.
pub fn card_for(ui: &Ui) -> GlassPanel {
    glass_panel(ui, GlassLevel::Base)
}

/// An inset panel that follows the active palette.
pub fn inset_for(ui: &Ui) -> GlassPanel {
    glass_panel(ui, GlassLevel::Secondary)
        .inner_margin(Margin::same(theme::SPACE_3 as i8))
        .corner_radius(theme::RADIUS_INPUT)
        .without_shadow()
}

/// One download row: a rounded, optionally-selected container.
///
/// The row is pinned to the parent's width; long content must truncate itself
/// ([`ellipsized_label`]) instead of widening the row past its card.
pub fn download_row<R>(ui: &mut Ui, selected: bool, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let palette = theme::palette(ui);
    let panel = if selected {
        glass_panel(ui, GlassLevel::Base)
            .fill(palette.accent.gamma_multiply(0.14))
            .stroke(Stroke::new(1.0, palette.accent.gamma_multiply(0.7)))
    } else {
        glass_panel(ui, GlassLevel::Secondary)
    };
    let inner_width = (ui.available_width() - 2.0 * theme::SPACE_3).max(0.0);
    panel
        .inner_margin(Margin::symmetric(
            theme::SPACE_3 as i8,
            theme::SPACE_2 as i8,
        ))
        .corner_radius(theme::RADIUS_PANEL)
        .without_shadow()
        .show(ui, |ui| {
            ui.set_width(inner_width);
            add_contents(ui)
        })
        .inner
}

// ---------------------------------------------------------------------------
// Typography
// ---------------------------------------------------------------------------

/// Page header: title, one-line muted description and an optional right-aligned
/// primary action.
///
/// Returns `true` when the action button was clicked.
pub fn page_header(ui: &mut Ui, title: &str, description: &str, action: Option<&str>) -> bool {
    let palette = theme::palette(ui);
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new(title)
                    .size(theme::SECTION_SIZE + 1.0)
                    .strong()
                    .color(palette.text),
            );
            if !description.is_empty() {
                ui.add(
                    egui::Label::new(RichText::new(description).size(12.5).color(palette.muted))
                        .truncate(),
                )
                .on_hover_text(description);
            }
        });
        if let Some(action) = action {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                clicked = primary_button(ui, action).clicked();
            });
        }
    });
    ui.add_space(theme::SPACE_1);
    clicked
}

/// Section heading inside a card.
pub fn section_title(ui: &mut Ui, text: &str) {
    let palette = theme::palette(ui);
    ui.label(
        RichText::new(text)
            .size(theme::SECTION_SIZE)
            .strong()
            .color(palette.text),
    );
}

/// Alias for [`section_title`] with the name the redesign stages use.
pub fn section_heading(ui: &mut Ui, text: &str) {
    section_title(ui, text);
}

/// Muted supporting text.
pub fn muted(ui: &mut Ui, text: impl Into<String>) {
    let palette = theme::palette(ui);
    ui.label(RichText::new(text.into()).size(12.0).color(palette.muted));
}

/// A single-line label that ends with an ellipsis instead of painting past the
/// row it belongs to.
///
/// Add it *after* the trailing badges/actions (inside a right-to-left layout)
/// so the truncation width is what actually remains.
pub fn ellipsized_label(ui: &mut Ui, text: RichText, tooltip: &str) -> Response {
    let response = ui.add(egui::Label::new(text).truncate());
    if tooltip.is_empty() {
        response
    } else {
        response.on_hover_text(tooltip)
    }
}

/// [`ellipsized_label`] capped to an explicit width, for table cells.
pub fn ellipsized_width(ui: &mut Ui, text: RichText, tooltip: &str, width: f32) -> Response {
    let response = ui
        .scope(|ui| {
            ui.set_max_width(width);
            ui.add(egui::Label::new(text).truncate())
        })
        .inner;
    if tooltip.is_empty() {
        response
    } else {
        response.on_hover_text(tooltip)
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Compact status pill: a coloured dot plus a label, so state is never
/// communicated by colour alone.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.55)))
        .inner_margin(Margin::symmetric(theme::SPACE_2 as i8, 3))
        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.5, color);
                ui.label(RichText::new(text).size(theme::SMALL_SIZE).color(color));
            });
        });
}

/// Status badge (explicit alias for the redesign stages).
pub fn status_badge(ui: &mut Ui, text: &str, color: Color32) {
    badge(ui, text, color);
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// Accent-filled call to action.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    action_button(ui, text, Action::Primary)
}

/// Neutral button for secondary actions.
pub fn secondary_button(ui: &mut Ui, text: &str) -> Response {
    action_button(ui, text, Action::Secondary)
}

/// Button for destructive actions (remove / cancel / clear).
pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    action_button(ui, text, Action::Cancel)
}

/// Text link style button.
pub fn link_button(ui: &mut Ui, text: &str) -> Response {
    let palette = theme::palette(ui);
    ui.add(
        Button::new(
            RichText::new(text)
                .size(theme::BODY_SIZE)
                .color(palette.accent_hover),
        )
        .frame(false),
    )
}

/// General semantic button. Only primary/add actions are filled; everything
/// else is quiet but carries its semantic tint.
pub fn button(ui: &mut Ui, text: &str, action: Action) -> Response {
    action_button(ui, text, action)
}

fn action_button(ui: &mut Ui, text: &str, action: Action) -> Response {
    let palette = theme::palette(ui);
    let enabled = ui.is_enabled();

    let (base, hover, pressed, foreground, border) = if !enabled {
        (
            palette.elevated,
            palette.elevated,
            palette.elevated,
            palette.disabled,
            palette.border,
        )
    } else {
        match action {
            Action::Primary | Action::Add => (
                palette.accent,
                palette.accent_hover,
                palette.accent_hover,
                palette.on_accent,
                palette.accent_hover,
            ),
            Action::Disabled => (
                palette.elevated,
                palette.elevated,
                palette.elevated,
                palette.disabled,
                palette.border,
            ),
            other => {
                let color = palette.action(other);
                (
                    palette.elevated,
                    palette.hover,
                    palette.border,
                    color,
                    color.gamma_multiply(0.55),
                )
            }
        }
    };

    styled_button(ui, text, base, hover, pressed, foreground, border)
}

fn styled_button(
    ui: &mut Ui,
    text: &str,
    base: Color32,
    hover: Color32,
    pressed: Color32,
    foreground: Color32,
    border: Color32,
) -> Response {
    ui.scope(|ui| {
        {
            let widgets = &mut ui.style_mut().visuals.widgets;
            widgets.inactive.weak_bg_fill = base;
            widgets.inactive.bg_fill = base;
            widgets.inactive.bg_stroke = Stroke::new(1.0, border);
            widgets.hovered.weak_bg_fill = hover;
            widgets.hovered.bg_fill = hover;
            widgets.hovered.bg_stroke = Stroke::new(1.0, border);
            widgets.active.weak_bg_fill = pressed;
            widgets.active.bg_fill = pressed;
            widgets.active.bg_stroke = Stroke::new(1.0, border);
        }
        ui.add(
            Button::new(RichText::new(text).size(theme::BODY_SIZE).color(foreground))
                .min_size(Vec2::new(0.0, 32.0)),
        )
    })
    .inner
}

/// Square icon button carrying an explicit semantic tint.
pub fn icon_button_colored(ui: &mut Ui, icon: &str, tooltip: &str, color: Color32) -> Response {
    let palette = theme::palette(ui);
    let response = ui
        .scope(|ui| {
            {
                let widgets = &mut ui.style_mut().visuals.widgets;
                widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
                widgets.inactive.bg_fill = Color32::TRANSPARENT;
                widgets.inactive.bg_stroke = Stroke::NONE;
                widgets.hovered.weak_bg_fill = palette.hover;
                widgets.hovered.bg_fill = palette.hover;
                widgets.hovered.bg_stroke = Stroke::new(1.0, palette.border);
                widgets.active.weak_bg_fill = palette.border;
                widgets.active.bg_fill = palette.border;
            }
            ui.add(
                Button::new(RichText::new(icon).size(15.0).color(color))
                    .frame(false)
                    .min_size(Vec2::splat(28.0)),
            )
        })
        .inner;
    response.on_hover_text(tooltip)
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Single-line search field with a hint and a fixed width.
pub fn search_input(
    ui: &mut Ui,
    text: &mut dyn egui::TextBuffer,
    hint: &str,
    width: f32,
) -> Response {
    ui.add(
        TextEdit::singleline(text)
            .hint_text(hint)
            .desired_width(width),
    )
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Download progress bar; `None` renders an indeterminate animation.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>, color: Color32, width: f32) -> Response {
    let track = theme::palette(ui).surface_secondary;
    let bar = match fraction {
        Some(value) => ProgressBar::new(value.clamp(0.0, 1.0)),
        None => ProgressBar::new(0.0).animate(true),
    };
    ui.scope(|ui| {
        ui.style_mut().visuals.extreme_bg_color = track;
        ui.add(bar.desired_width(width).fill(color))
    })
    .inner
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// A statistics tile: caption, metric, semantic accent and an optional action.
///
/// Clickable tiles get the same affordances as buttons: a pointing-hand cursor,
/// an accent outline on hover/focus and keyboard activation with Enter/Space.
pub fn stat_tile<F>(
    ui: &mut Ui,
    label: &str,
    value: String,
    accent: Color32,
    on_click: Option<F>,
) -> Response
where
    F: FnOnce(),
{
    let palette = theme::palette(ui);
    let card = card_for(ui).show(ui, |ui| {
        ui.set_min_width(140.0);
        ui.label(
            RichText::new(label)
                .size(theme::SMALL_SIZE)
                .color(palette.text_secondary),
        );
        ui.add_space(theme::SPACE_1);
        ui.label(RichText::new(value).size(theme::METRIC_SIZE).color(accent));
    });

    let id = ui.scope_id().with(("stat_tile", label));
    let Some(on_click) = on_click else {
        return ui.interact(card.response.rect, id, Sense::hover());
    };

    let response = ui.interact(card.response.rect, id, Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);

    if response.hovered() || response.has_focus() {
        ui.painter().rect_stroke(
            card.response.rect,
            CornerRadius::same(theme::RADIUS_PANEL),
            Stroke::new(1.0, palette.accent_hover),
            egui::StrokeKind::Inside,
        );
    }

    let keyboard_activated = response.has_focus()
        && ui.input(|input| {
            input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Space)
        });
    if response.clicked() || keyboard_activated {
        on_click();
    }

    response
}

// ---------------------------------------------------------------------------
// Empty states
// ---------------------------------------------------------------------------

/// Empty state with a title, supporting copy and an optional action button.
/// Returns `true` when the action was clicked.
pub fn empty_state_action(ui: &mut Ui, title: &str, subtitle: &str, action: &str) -> bool {
    let palette = theme::palette(ui);
    let mut clicked = false;
    ui.add_space(theme::SPACE_7);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).size(15.0).color(palette.text));
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).size(12.5).color(palette.muted));
        }
        if !action.is_empty() {
            ui.add_space(theme::SPACE_3);
            clicked = primary_button(ui, action).clicked();
        }
    });
    ui.add_space(theme::SPACE_7);
    clicked
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

/// Result of a rendered confirmation dialog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DialogResult {
    pub accepted: bool,
    pub cancelled: bool,
}

/// Confirmation dialog body: title, message, optional extra content, and the
/// accept/cancel pair. Destructive dialogs use the danger button.
pub fn confirmation_dialog(
    ui: &mut Ui,
    width: f32,
    title: &str,
    message: &str,
    accept_label: &str,
    destructive: bool,
    body: impl FnOnce(&mut Ui),
) -> DialogResult {
    let palette = theme::palette(ui);
    let mut result = DialogResult::default();
    glass_panel(ui, GlassLevel::Elevated)
        .inner_margin(Margin::same(theme::SPACE_5 as i8))
        .corner_radius(theme::RADIUS_DIALOG)
        .show(ui, |ui| {
            ui.set_width(width);
            ui.label(
                RichText::new(title)
                    .size(theme::SECTION_SIZE)
                    .strong()
                    .color(palette.text),
            );
            ui.add_space(theme::SPACE_1);
            ui.label(
                RichText::new(message)
                    .size(theme::BODY_SIZE)
                    .color(palette.text_secondary),
            );
            ui.add_space(theme::SPACE_2);
            body(ui);
            ui.add_space(theme::SPACE_3);
            ui.horizontal(|ui| {
                let accept = if destructive {
                    danger_button(ui, accept_label)
                } else {
                    primary_button(ui, accept_label)
                };
                result.accepted = accept.clicked();
                result.cancelled = secondary_button(ui, "Cancel").clicked();
            });
        });
    result
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

/// One toast row: a coloured edge plus the message.
///
/// The message wraps inside the toast instead of widening it, so a long engine
/// error stays fully readable and inside the reserved strip.
pub fn toast(ui: &mut Ui, notification: &Notification) {
    let palette = theme::palette(ui);
    let color = palette.severity_color(notification.severity);
    let inner_width =
        (ui.available_width() - 2.0 * theme::SPACE_3 - 8.0 - theme::SPACE_2).max(80.0);
    glass_panel(ui, GlassLevel::Elevated)
        .inner_margin(Margin::symmetric(theme::SPACE_3 as i8, 6))
        .corner_radius(theme::RADIUS_INPUT)
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.8)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, color);
                ui.scope(|ui| {
                    ui.set_max_width(inner_width);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&notification.text)
                                .size(13.0)
                                .color(palette.text),
                        )
                        .wrap(),
                    );
                });
            });
        });
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// A row label + value pair used inside the detail grid.
pub fn detail_row(ui: &mut Ui, label: &str, value: &str) {
    let palette = theme::palette(ui);
    ui.label(
        RichText::new(label)
            .size(12.0)
            .color(palette.text_secondary),
    );
    ui.label(RichText::new(value).size(12.0).color(palette.text));
    ui.end_row();
}

/// Diagnostics block: semantic headline plus label/value rows.
pub fn diagnostic_panel(ui: &mut Ui, headline: &str, color: Color32, details: &[(&str, &str)]) {
    let palette = theme::palette(ui);
    inset_for(ui).show(ui, |ui| {
        ui.label(RichText::new(headline).size(12.0).strong().color(color));
        for (label, value) in details {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(*label)
                        .size(theme::SMALL_SIZE)
                        .color(palette.muted),
                );
                ui.label(
                    RichText::new(*value)
                        .size(theme::SMALL_SIZE)
                        .monospace()
                        .color(palette.text_secondary),
                );
            });
        }
    });
}

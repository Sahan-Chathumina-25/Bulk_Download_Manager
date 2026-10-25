//! Reusable widgets shared by the six pages.
//!
//! Kept deliberately small: each helper takes a `Ui` and paints one element, so
//! pages stay declarative and layout changes are local.

use egui::{Color32, Frame, Margin, Response, RichText, Stroke, Ui};

use crate::app::state::Notification;
use crate::app::theme;

/// A raised card: the default container for grouped content.
pub fn card() -> Frame {
    Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .inner_margin(Margin::same(14))
        .corner_radius(egui::CornerRadius::same(10))
}

/// A flat inset panel used for inputs and list backgrounds.
///
/// Not yet used by a page; kept because it belongs to the shared widget set.
#[allow(dead_code)]
pub fn inset() -> Frame {
    Frame::new()
        .fill(theme::SURFACE_ALT)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .inner_margin(Margin::same(10))
        .corner_radius(egui::CornerRadius::same(8))
}

/// Page heading with an optional muted subtitle.
pub fn page_heading(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(24.0).strong().color(theme::TEXT));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(theme::MUTED));
    }
    ui.add_space(4.0);
}

/// Small section caption inside a card.
pub fn section_title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(15.0).strong().color(theme::TEXT));
}

/// Muted body text.
pub fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(theme::MUTED));
}

/// Compact coloured pill, e.g. a download state.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.22))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .inner_margin(Margin::symmetric(8, 2))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(color));
        });
}

/// Accent-filled call to action.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE).strong())
            .fill(theme::ACCENT)
            .stroke(Stroke::NONE)
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

/// Neutral button for secondary actions.
pub fn secondary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(theme::TEXT))
            .fill(theme::SURFACE_ALT)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

/// Button for destructive actions (remove / cancel / clear).
pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(theme::ERROR))
            .fill(theme::SURFACE_ALT)
            .stroke(Stroke::new(1.0, theme::ERROR.gamma_multiply(0.7)))
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

/// Text link style button.
pub fn link_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(theme::ACCENT_HOVER)).frame(false))
}

/// Centred placeholder for an empty list.
pub fn empty_state(ui: &mut Ui, text: &str) {
    ui.add_space(28.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).color(theme::MUTED).size(14.0));
    });
    ui.add_space(28.0);
}

/// Download progress bar with an unknown-length animation.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>, color: Color32, width: f32) -> Response {
    let bar = match fraction {
        Some(value) => egui::ProgressBar::new(value.clamp(0.0, 1.0)),
        None => egui::ProgressBar::new(0.0).animate(true),
    };
    ui.add(bar.desired_width(width).fill(color))
}

/// One toast row: a coloured edge plus the message.
pub fn toast(ui: &mut Ui, notification: &Notification) {
    let color = theme::severity_color(notification.severity);
    Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.8)))
        .inner_margin(Margin::symmetric(10, 6))
        .corner_radius(egui::CornerRadius::same(6))
        .show(ui, |ui| {
            ui.label(
                RichText::new(&notification.text)
                    .color(theme::TEXT)
                    .size(13.0),
            );
        });
}

/// A row label + value pair used inside the detail grid.
pub fn detail_row(ui: &mut Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).color(theme::MUTED).size(12.0));
    ui.label(RichText::new(value).color(theme::TEXT).size(12.0));
    ui.end_row();
}

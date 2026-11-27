//! Small GPUI building blocks shared by the page views.
//!
//! These are deliberately plain `div` compositions rather than custom
//! elements: every control stays focusable, sized, and exposed to the
//! accessibility tree through GPUI Kit's own components.

use gpui_kit::component::progress::Progress;
use gpui_kit::*;

use crate::app::theme::Palette;

/// Page heading with a short, task-oriented subtitle.
pub fn page_header(title: impl Into<SharedString>, subtitle: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(22.0))
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(crate::app::theme::Palette::light().text_muted)
                .child(subtitle.into()),
        )
}

/// A short section label. Text color is supplied by the caller so it adapts
/// to the current appearance.
pub fn section_title(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(13.0))
        .text_color(color)
        .child(text.into())
}

/// Secondary text.
pub fn muted(text: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_size(px(12.0)).text_color(color).child(text.into())
}

/// Elevated surface with a hairline border.
pub fn card(content: impl IntoElement, palette: Palette) -> Div {
    div()
        .rounded(px(12.0))
        .border_1()
        .border_color(palette.border)
        .bg(palette.surface)
        .p(px(16.0))
        .child(content)
}

/// A status pill. The text label is always shown beside the color.
pub fn badge(text: impl Into<SharedString>, color: Hsla, palette: Palette) -> Div {
    div()
        .rounded(px(6.0))
        .border_1()
        .border_color(color)
        .bg(palette.surface_alt)
        .px(px(8.0))
        .py(px(2.0))
        .text_size(px(11.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(text.into())
}

/// A key/value row used in detail panels.
pub fn detail_row(label: impl Into<SharedString>, value: impl Into<SharedString>, muted_color: Hsla) -> Div {
    div()
        .flex()
        .gap(px(12.0))
        .py(px(2.0))
        .child(
            div()
                .w(px(110.0))
                .flex_none()
                .text_size(px(12.0))
                .text_color(muted_color)
                .child(label.into()),
        )
        .child(div().text_size(px(12.0)).child(value.into()))
}

/// Determinate or indeterminate progress bar.
pub fn progress_bar(id: impl Into<ElementId>, fraction: Option<f32>, color: Hsla) -> Progress {
    let bar = Progress::new(id);
    match fraction {
        Some(value) => bar.value((value.clamp(0.0, 1.0) * 100.0) as f32).color(color),
        None => bar.loading(true).color(color),
    }
}

/// Empty-state prompt with an explicit next action described in the subtitle.
pub fn empty_state(title: impl Into<SharedString>, subtitle: impl Into<SharedString>, palette: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .py(px(48.0))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(13.0))
                .text_color(palette.text)
                .child(title.into()),
        )
        .child(muted(subtitle, palette.text_muted))
}

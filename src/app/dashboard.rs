//! Dashboard: a quiet summary of what is transferring and what needs attention.

use gpui_kit::component::button::Button;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{App, components, format, theme};
use crate::models::download_status::DownloadState;

pub fn show(app: &App, _window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(_window, cx);
    let stats = app.dashboard.stats;

    let stat = |label: &str, value: String, color: Hsla| {
        div()
            .flex_1()
            .rounded(px(10.0))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(20.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color)
                    .child(value),
            )
            .child(components::muted(label, palette.text_muted))
    };

    let mut column = div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(components::page_header(
            "Dashboard",
            "A live view of transfers, failures, and recent activity.",
        ))
        .child(
            div()
                .flex()
                .gap(px(12.0))
                .child(stat("Active", stats.active.to_string(), palette.accent))
                .child(stat("Queued", stats.queued.to_string(), palette.text))
                .child(stat("Completed", stats.completed.to_string(), palette.success))
                .child(stat("Failed", stats.failed.to_string(), palette.danger)),
        );

    let active: Vec<_> = app
        .queue
        .records()
        .iter()
        .filter(|record| theme::is_active(record.state))
        .collect();

    column = column.child(
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(components::section_title("Active transfers", palette.text))
            .children(active.iter().map(|record| {
                let view = app.queue.view_for(record);
                let color = theme::state_color(record.state, palette);
                div()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.surface)
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(13.0)).child(record.display_name.clone()))
                            .child(components::badge(theme::state_label(record.state), color, palette)),
                    )
                    .child(components::progress_bar(
                        format!("dash-progress-{}", record.id.unwrap_or(-1)),
                        view.fraction,
                        color,
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.0))
                            .text_color(palette.text_muted)
                            .child(format::format_bytes(Some(view.downloaded_bytes)))
                            .child(format_speed_label(view.speed_bps)),
                    )
            }))
            .when(active.is_empty(), |el| {
                el.child(components::empty_state(
                    "No active transfers",
                    "Add a URL to start a download.",
                    palette,
                ))
            }),
    );

    let failed: Vec<_> = app
        .queue
        .records()
        .iter()
        .filter(|record| record.state == DownloadState::Failed)
        .take(5)
        .collect();

    if !failed.is_empty() {
        column = column.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(components::section_title("Needs attention", palette.danger))
                .children(failed.iter().map(|record| {
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(palette.border)
                        .bg(palette.surface)
                        .p(px(10.0))
                        .child(div().text_size(px(12.0)).child(record.display_name.clone()))
                        .child(components::muted(
                            record.error_summary.clone().unwrap_or_else(|| "Download failed".into()),
                            palette.danger,
                        ))
                })),
        );
    }

    column = column.child(
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(components::section_title("Recent activity", palette.text))
            .children(app.dashboard.recent.iter().take(5).map(|record| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .py(px(4.0))
                    .text_size(px(12.0))
                    .child(record.display_name.clone())
                    .child(components::badge(
                        theme::state_label(record.state),
                        theme::state_color(record.state, palette),
                        palette,
                    ))
            }))
            .when(app.dashboard.recent.is_empty(), |el| {
                el.child(components::muted("No history yet.", palette.text_muted))
            }),
    );

    column.child(
        Button::new("dashboard-refresh")
            .label("Refresh")
            .on_click(cx.listener(|this, _, _, cx| {
                this.request_dashboard();
                cx.notify();
            })),
    )
}

fn format_speed_label(speed: f64) -> String {
    if speed <= 0.0 {
        "—".to_string()
    } else {
        format::format_speed(Some(speed))
    }
}

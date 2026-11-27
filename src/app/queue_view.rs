//! Queue: the live control surface for downloads.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{Disableable as _, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{App, components, format, theme};
use crate::app::backend::{Command, QueueAction};
use crate::app::state::Confirmation;
use crate::models::download_status::DownloadState;

pub fn show(app: &App, _window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(_window, cx);
    let selected = app.queue.selected_ids();

    let action_button = |id: &'static str, label: &'static str, action: Option<QueueAction>| {
        let enabled = action.is_some();
        Button::new(id)
            .label(label)
            .when(enabled, |button| button.primary())
            .when(!enabled, |button| button.disabled(true))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(action) = action {
                    let ids = this.queue.selected_ids();
                    if !ids.is_empty() {
                        this.send(Command::QueueAction { ids, action });
                        this.queue.clear_selection();
                        cx.notify();
                    }
                }
            }))
    };

    let can_pause = app.queue.records().iter().any(|record| {
        app.queue.selected().contains(&record.id.unwrap_or(-1))
            && matches!(record.state, DownloadState::Connecting | DownloadState::Downloading)
    });
    let can_resume = app.queue.records().iter().any(|record| {
        app.queue.selected().contains(&record.id.unwrap_or(-1))
            && matches!(
                record.state,
                DownloadState::Paused | DownloadState::Interrupted | DownloadState::Queued
            )
    });
    let can_retry = app.queue.records().iter().any(|record| {
        app.queue.selected().contains(&record.id.unwrap_or(-1))
            && matches!(
                record.state,
                DownloadState::Failed
                    | DownloadState::Cancelled
                    | DownloadState::Interrupted
                    | DownloadState::Paused
            )
    });

    let toolbar = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .pb(px(12.0))
        .child(
            Button::new("queue-start-all")
                .icon(IconName::Play)
                .label("Start all")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.send(Command::StartAll);
                    cx.notify();
                })),
        )
        .child(action_button("queue-pause", "Pause", can_pause.then_some(QueueAction::Pause)))
        .child(action_button("queue-resume", "Resume", can_resume.then_some(QueueAction::Resume)))
        .child(action_button("queue-retry", "Retry", can_retry.then_some(QueueAction::Retry)))
        .child(
            Button::new("queue-cancel")
                .label("Cancel")
                .disabled(selected.is_empty())
                .on_click(cx.listener(|this, _, _, cx| {
                    let ids = this.queue.selected_ids();
                    if !ids.is_empty() {
                        this.confirm(Confirmation::CancelQueue { ids });
                        cx.notify();
                    }
                })),
        )
        .child(
            Button::new("queue-remove")
                .label("Remove")
                .disabled(selected.is_empty())
                .on_click(cx.listener(|this, _, _, cx| {
                    let ids = this.queue.selected_ids();
                    if !ids.is_empty() {
                        this.confirm(Confirmation::RemoveQueue { ids });
                        cx.notify();
                    }
                })),
        );

    let rows = app.queue.records().iter().map(|record| {
        let id = record.id.unwrap_or(-1);
        let checked = app.queue.selected().contains(&id);
        let view = app.queue.view_for(record);
        let color = theme::state_color(record.state, palette);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .p(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        Checkbox::new(format!("queue-select-{id}"))
                            .checked(checked)
                            .accessibility_label("Select download")
                            .on_change(cx.listener(move |this, value, _, cx| {
                                this.queue.set_selected(id, *value);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .child(record.display_name.clone()),
                    )
                    .child(components::badge(theme::state_label(record.state), color, palette)),
            )
            .child(components::progress_bar(
                format!("queue-progress-{id}"),
                view.fraction,
                color,
            ))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(11.0))
                    .text_color(palette.text_muted)
                    .child(format!(
                        "{} / {}",
                        format::format_bytes(Some(view.downloaded_bytes)),
                        format::format_bytes(view.total_bytes)
                    ))
                    .child(format_speed(view.speed_bps, view.eta_seconds)),
            )
    });

    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(components::page_header("Queue", "Control active and queued downloads."))
        .child(toolbar)
        .children(rows)
        .when(app.queue.is_empty(), |el| {
            el.child(components::empty_state(
                "The queue is empty",
                "Use Add Downloads to bring files in.",
                palette,
            ))
        })
}

fn format_speed(speed: f64, eta: Option<u64>) -> String {
    if speed <= 0.0 {
        return "—".to_string();
    }
    format!(
        "{} · ETA {}",
        format::format_speed(Some(speed)),
        format::format_eta(eta)
    )
}

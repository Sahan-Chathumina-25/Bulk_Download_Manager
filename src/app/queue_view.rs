//! Queue: the live control surface for downloads.

use dioxus::prelude::*;

use crate::app::{components, format, theme, use_ui};
use crate::app::backend::{Command, QueueAction};
use crate::app::state::Confirmation;
use crate::models::download_status::DownloadState;

struct QueueRow {
    id: i64,
    name: String,
    state: String,
    tone: &'static str,
    checked: bool,
    fraction: Option<f32>,
    bytes: String,
    speed: String,
}

#[component]
pub fn Queue() -> Element {
    let mut ui = use_ui();

    let (can_pause, can_resume, can_retry, has_selection, empty, rows) = {
        let model = ui.model.read();
        let selected = model.queue.selected();
        let records = model.queue.records();
        let can = |states: &[DownloadState]| {
            records.iter().any(|record| {
                selected.contains(&record.id.unwrap_or(-1)) && states.contains(&record.state)
            })
        };
        let rows: Vec<QueueRow> = records
            .iter()
            .map(|record| {
                let id = record.id.unwrap_or(-1);
                let view = model.queue.view_for(record);
                QueueRow {
                    id,
                    name: record.display_name.clone(),
                    state: theme::state_label(record.state).to_string(),
                    tone: theme::state_class(record.state),
                    checked: selected.contains(&id),
                    fraction: view.fraction,
                    bytes: format!(
                        "{} / {}",
                        format::format_bytes(Some(view.downloaded_bytes)),
                        format::format_bytes(view.total_bytes)
                    ),
                    speed: speed_label(view.speed_bps, view.eta_seconds),
                }
            })
            .collect();
        (
            can(&[DownloadState::Connecting, DownloadState::Downloading]),
            can(&[
                DownloadState::Paused,
                DownloadState::Interrupted,
                DownloadState::Queued,
            ]),
            can(&[
                DownloadState::Failed,
                DownloadState::Cancelled,
                DownloadState::Interrupted,
                DownloadState::Paused,
            ]),
            !selected.is_empty(),
            model.queue.is_empty(),
            rows,
        )
    };

    rsx! {
        components::PageHeader {
            title: "Queue".to_string(),
            subtitle: "Control active and queued downloads.".to_string(),
        }
        div { class: "row",
            button {
                class: "primary",
                onclick: move |_| ui.model.write().send(Command::StartAll),
                "Start all"
            }
            button {
                disabled: !can_pause,
                onclick: move |_| apply_action(ui, QueueAction::Pause),
                "Pause"
            }
            button {
                disabled: !can_resume,
                onclick: move |_| apply_action(ui, QueueAction::Resume),
                "Resume"
            }
            button {
                disabled: !can_retry,
                onclick: move |_| apply_action(ui, QueueAction::Retry),
                "Retry"
            }
            button {
                disabled: !has_selection,
                onclick: move |_| confirm_ids(ui, false),
                "Cancel"
            }
            button {
                disabled: !has_selection,
                onclick: move |_| confirm_ids(ui, true),
                "Remove"
            }
        }
        for row in rows {
            article { class: "item",
                div { class: "row",
                    input {
                        r#type: "checkbox",
                        checked: row.checked,
                        onchange: move |event| {
                            ui.model.write().queue.set_selected(row.id, event.checked());
                        },
                    }
                    span { class: "grow", "{row.name}" }
                    components::Badge { text: row.state, tone: row.tone.to_string() }
                }
                components::ProgressBar { fraction: row.fraction }
                div { class: "row between muted",
                    span { "{row.bytes}" }
                    span { "{row.speed}" }
                }
            }
        }
        if empty {
            components::EmptyState {
                title: "The queue is empty".to_string(),
                subtitle: "Use Add Downloads to bring files in.".to_string(),
            }
        }
    }
}

fn apply_action(mut ui: crate::app::Ui, action: QueueAction) {
    let mut model = ui.model.write();
    let ids = model.queue.selected_ids();
    if !ids.is_empty() {
        model.send(Command::QueueAction { ids, action });
        model.queue.clear_selection();
    }
}

fn confirm_ids(mut ui: crate::app::Ui, remove: bool) {
    let mut model = ui.model.write();
    let ids = model.queue.selected_ids();
    if ids.is_empty() {
        return;
    }
    let confirmation = if remove {
        Confirmation::RemoveQueue { ids }
    } else {
        Confirmation::CancelQueue { ids }
    };
    model.confirm(confirmation);
}

fn speed_label(speed: f64, eta: Option<u64>) -> String {
    if speed <= 0.0 {
        return "—".to_string();
    }
    format!(
        "{} · ETA {}",
        format::format_speed(Some(speed)),
        format::format_eta(eta)
    )
}

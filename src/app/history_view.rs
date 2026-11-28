//! History: search, sort, page through finished transfers.

use dioxus::prelude::*;

use crate::app::{components, format, theme, use_ui};
use crate::app::backend::{Command, QueueAction};
use crate::app::state::Confirmation;
use crate::models::download_status::DownloadState;

struct HistoryRow {
    id: i64,
    name: String,
    state: String,
    tone: &'static str,
    bytes: String,
    completed: String,
    error: Option<String>,
    checked: bool,
}

#[component]
pub fn History() -> Element {
    let mut ui = use_ui();

    let (search, filters, rows, empty, offset, page, has_next, keep_files) = {
        let model = ui.model.read();
        let current = model.history.query.state.as_deref();
        let filters: Vec<(&'static str, Option<DownloadState>, bool)> = [
            ("All", None),
            ("Completed", Some(DownloadState::Completed)),
            ("Failed", Some(DownloadState::Failed)),
            ("Cancelled", Some(DownloadState::Cancelled)),
            ("Interrupted", Some(DownloadState::Interrupted)),
        ]
        .into_iter()
        .map(|(label, state)| {
            let active = match (current, state) {
                (None, None) => true,
                (Some(value), Some(expected)) => value == expected.as_str(),
                _ => false,
            };
            (label, state, active)
        })
        .collect();
        let rows: Vec<HistoryRow> = model
            .history
            .records
            .iter()
            .map(|record| {
                let id = record.id.unwrap_or(-1);
                HistoryRow {
                    id,
                    name: record.display_name.clone(),
                    state: theme::state_label(record.state).to_string(),
                    tone: theme::state_class(record.state),
                    bytes: format::format_bytes(Some(record.downloaded_bytes)),
                    completed: record.completed_at.clone().unwrap_or_else(|| "—".into()),
                    error: record.error_summary.clone(),
                    checked: model.history.selected.contains(&id),
                }
            })
            .collect();
        (
            model.history_search.clone(),
            filters,
            rows,
            model.history.records.is_empty(),
            model.history.query.offset,
            model.history.page_number(),
            model.history.has_next_page(),
            model.history.keep_files_on_clear,
        )
    };

    rsx! {
        components::PageHeader {
            title: "History".to_string(),
            subtitle: "Search finished downloads and retry failures.".to_string(),
        }
        div { class: "row",
            input {
                class: "wide",
                value: "{search}",
                placeholder: "Search files, paths, URLs",
                oninput: move |event| ui.model.write().history_search = event.value(),
            }
            button {
                onclick: move |_| {
                    let mut model = ui.model.write();
                    model.history.search_input = model.history_search.clone();
                    model.history.apply_search();
                    model.request_history();
                },
                "Search"
            }
            button {
                class: "ghost",
                onclick: move |_| ui.model.write().request_history(),
                "Refresh"
            }
        }
        div { class: "row",
            for (label, state, active) in filters {
                button {
                    class: if active { "active" } else { "ghost" },
                    onclick: move |_| {
                        let mut model = ui.model.write();
                        model.history.set_state_filter(state);
                        model.request_history();
                    },
                    "{label}"
                }
            }
        }
        for row in rows {
            article { class: "item",
                div { class: "row",
                    input {
                        r#type: "checkbox",
                        checked: row.checked,
                        onchange: move |event| {
                            let mut model = ui.model.write();
                            if event.checked() {
                                model.history.selected.insert(row.id);
                            } else {
                                model.history.selected.remove(&row.id);
                            }
                        },
                    }
                    span { class: "grow", "{row.name}" }
                    components::Badge { text: row.state, tone: row.tone.to_string() }
                }
                div { class: "row between muted",
                    span { "{row.bytes}" }
                    span { "{row.completed}" }
                }
                if let Some(error) = row.error {
                    components::Muted { text: error, tone: "danger".to_string() }
                }
            }
        }
        if empty {
            components::EmptyState {
                title: "No history yet".to_string(),
                subtitle: "Completed and failed downloads appear here.".to_string(),
            }
        }
        div { class: "row",
            button {
                class: "ghost",
                disabled: offset == 0,
                onclick: move |_| {
                    let mut model = ui.model.write();
                    model.history.previous_page();
                    model.request_history();
                },
                "Previous"
            }
            components::Muted { text: format!("Page {page}"), tone: "".to_string() }
            button {
                class: "ghost",
                disabled: !has_next,
                onclick: move |_| {
                    let mut model = ui.model.write();
                    model.history.next_page();
                    model.request_history();
                },
                "Next"
            }
            span { class: "grow" }
            button {
                onclick: move |_| {
                    let model = ui.model.write();
                    let ids: Vec<i64> = model.history.records.iter()
                        .filter(|record| record.state == DownloadState::Failed)
                        .filter_map(|record| record.id)
                        .collect();
                    if !ids.is_empty() {
                        model.send(Command::QueueAction { ids, action: QueueAction::Retry });
                    }
                },
                "Retry failed"
            }
            label { class: "checkbox",
                input {
                    r#type: "checkbox",
                    checked: keep_files,
                    onchange: move |event| ui.model.write().history.keep_files_on_clear = event.checked(),
                }
                "Also delete downloaded files"
            }
            button {
                class: "danger",
                onclick: move |_| ui.model.write().confirm(Confirmation::ClearHistory),
                "Clear history…"
            }
        }
    }
}

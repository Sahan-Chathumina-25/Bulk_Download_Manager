//! Discover: scan a page, review candidates, queue only what is selected.

use dioxus::prelude::*;

use crate::app::{components, format, use_ui};
use crate::app::backend::Command;
use crate::services::validation::redact_url;

struct CandidateRow {
    index: usize,
    filename: String,
    label: &'static str,
    checked: bool,
    selectable: bool,
    meta: String,
    url: String,
    filtered: Option<String>,
}

#[component]
pub fn Discover() -> Element {
    let mut ui = use_ui();

    let (url, scanning, status, include_unknown, selected, empty, destination, rows) = {
        let model = ui.model.read();
        let selected = model.discovery.selected_count();
        let rows: Vec<CandidateRow> = model
            .discovery
            .visible_indices()
            .into_iter()
            .map(|index| {
                let candidate = &model.discovery.candidates()[index];
                CandidateRow {
                    index,
                    filename: candidate.filename.clone(),
                    label: if candidate.is_likely_file { "Direct file" } else { "Web page" },
                    checked: model.discovery.is_selected(index),
                    selectable: candidate.filtered_reason.is_none(),
                    meta: format!(
                        "{} · {} · {}",
                        candidate.extension.clone().unwrap_or_else(|| "—".into()),
                        format::format_bytes(candidate.size),
                        candidate.content_type.clone().unwrap_or_else(|| "—".into()),
                    ),
                    url: redact_url(&candidate.candidate_url),
                    filtered: candidate.filtered_reason.clone(),
                }
            })
            .collect();
        let destination = destination_label(&model.settings.default_destination_dir, model.discovery_destination.as_ref());
        (
            model.discovery_url.clone(),
            model.discovery.scanning,
            model.discovery.status.clone(),
            model.discovery.include_unknown,
            selected,
            model.discovery.candidates().is_empty(),
            destination,
            rows,
        )
    };

    rsx! {
        components::PageHeader {
            title: "Discover".to_string(),
            subtitle: "Scan a page, review links, then add only what you choose.".to_string(),
        }
        div { class: "row",
            input {
                class: "wide",
                value: "{url}",
                placeholder: "https://example.com/page",
                oninput: move |event| ui.model.write().discovery_url = event.value(),
            }
            button {
                class: "primary",
                disabled: scanning,
                onclick: move |_| {
                    let mut model = ui.model.write();
                    let page = model.discovery_url.clone();
                    model.discovery.page_url = page.clone();
                    model.discovery.scanning = true;
                    model.send(Command::ScanPage { url: page });
                },
                "Scan page"
            }
            button {
                class: "ghost",
                disabled: !scanning,
                onclick: move |_| {
                    let mut model = ui.model.write();
                    model.send(Command::CancelScan);
                    model.discovery.scanning = false;
                },
                "Cancel"
            }
        }
        components::Muted { text: status, tone: "".to_string() }
        div { class: "row",
            label { class: "checkbox",
                input {
                    r#type: "checkbox",
                    checked: include_unknown,
                    onchange: move |event| ui.model.write().discovery.include_unknown = event.checked(),
                }
                "Include unknown types"
            }
            button {
                class: "ghost",
                onclick: move |_| ui.model.write().discovery.set_all(true),
                "Select all"
            }
            button {
                class: "ghost",
                onclick: move |_| ui.model.write().discovery.set_all(false),
                "Deselect all"
            }
        }
        for row in rows {
            article { class: "item",
                div { class: "row",
                    input {
                        r#type: "checkbox",
                        checked: row.checked && row.selectable,
                        disabled: !row.selectable,
                        onchange: move |_| ui.model.write().discovery.toggle(row.index),
                    }
                    span { class: "grow", "{row.filename}" }
                    components::Badge { text: row.label.to_string(), tone: "accent".to_string() }
                }
                components::Muted { text: row.meta, tone: "".to_string() }
                components::Muted { text: row.url, tone: "".to_string() }
                if let Some(reason) = row.filtered {
                    components::Muted { text: reason, tone: "danger".to_string() }
                }
            }
        }
        if empty {
            components::EmptyState {
                title: "No scan results".to_string(),
                subtitle: "Enter a page URL and choose Scan page.".to_string(),
            }
        }
        div { class: "row",
            button {
                onclick: move |_| {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        ui.model.write().discovery_destination = Some(folder);
                    }
                },
                "Destination"
            }
            span { class: "muted", "{destination}" }
            span { class: "grow" }
            button {
                class: "primary",
                disabled: selected == 0,
                onclick: move |_| {
                    let model = ui.model.write();
                    let candidates = model.discovery.candidates_for_planner();
                    let destination = model.destination(model.discovery_destination.as_ref());
                    model.send(Command::EnqueueCandidates { candidates, destination });
                },
                "Add {selected} selected"
            }
        }
    }
}

fn destination_label(configured: &str, selected: Option<&std::path::PathBuf>) -> String {
    match selected {
        Some(path) => path.to_string_lossy().into_owned(),
        None if configured.trim().is_empty() => "Default download folder".to_string(),
        None => configured.trim().to_string(),
    }
}

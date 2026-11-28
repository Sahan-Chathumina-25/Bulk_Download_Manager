//! AI File Finder: offline, advisory archive detection over pasted URLs.

use dioxus::prelude::*;

use crate::app::{components, format, use_ui};
use crate::app::backend::Command;
use crate::services::validation::redact_url;

struct CandidateRow {
    index: usize,
    filename: String,
    status: String,
    tone: &'static str,
    meta: String,
    url: String,
    checked: bool,
    open: bool,
    evidence: Vec<String>,
    missing: Vec<String>,
}

#[component]
pub fn AiFinder() -> Element {
    let mut ui = use_ui();

    let (urls, analyzing, status, from_discover, probe, report_stats, selected, empty, incomplete, rows) = {
        let model = ui.model.read();
        let rows: Vec<CandidateRow> = model
            .ai
            .candidates()
            .iter()
            .enumerate()
            .map(|(index, candidate)| CandidateRow {
                index,
                filename: candidate.filename.clone(),
                status: format!("{} · {}", candidate.score, candidate.status),
                tone: if candidate.score >= 75 {
                    "success"
                } else if candidate.score >= 50 {
                    "accent"
                } else {
                    "muted"
                },
                meta: format!(
                    "{} · confidence {} · {}",
                    candidate.archive_type,
                    candidate.confidence.label(),
                    format::format_bytes(candidate.size),
                ),
                url: redact_url(&candidate.url),
                checked: model.ai.is_selected(index),
                open: model.ai.expanded.contains(&index),
                evidence: candidate
                    .evidence
                    .iter()
                    .map(|item| format!("{:+} {}", item.weight, item.reason))
                    .collect(),
                missing: candidate
                    .missing_evidence
                    .iter()
                    .map(|item| format!("Missing: {item}"))
                    .collect(),
            })
            .collect();
        let report_stats = model.ai.report.as_ref().map(|report| {
            (
                report.analyzed_count,
                report.candidate_count,
                report.filtered_count,
            )
        });
        (
            model.ai.urls_text.clone(),
            model.ai.analyzing,
            model.ai.status.clone(),
            model.ai.analyze_discovered,
            model.ai.probe_metadata,
            report_stats,
            model.ai.selected_count(),
            model.ai.candidates().is_empty(),
            model.ai.selected_has_incomplete_groups(),
            rows,
        )
    };

    rsx! {
        components::PageHeader {
            title: "AI File Finder".to_string(),
            subtitle: "Offline archive suggestions — verify before adding anything.".to_string(),
        }
        section { class: "column",
            textarea {
                value: "{urls}",
                placeholder: "Paste URLs to inspect…",
                oninput: move |event| ui.model.write().ai.urls_text = event.value(),
            }
            div { class: "row",
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: from_discover,
                        onchange: move |event| ui.model.write().ai.analyze_discovered = event.checked(),
                    }
                    "Use links from the last Discover scan"
                }
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: probe,
                        onchange: move |event| ui.model.write().ai.probe_metadata = event.checked(),
                    }
                    "Verify with a small HTTP check"
                }
                button {
                    class: "primary",
                    disabled: analyzing,
                    onclick: move |_| find_files(ui),
                    "Find files"
                }
                button {
                    class: "ghost",
                    disabled: !analyzing,
                    onclick: move |_| {
                        let mut model = ui.model.write();
                        model.send(Command::CancelAnalysis);
                        model.ai.analyzing = false;
                    },
                    "Cancel"
                }
            }
            components::Muted { text: status, tone: "".to_string() }
        }
        if let Some((analyzed, candidates, filtered)) = report_stats {
            div { class: "row",
                components::Muted { text: format!("{analyzed} analyzed"), tone: "".to_string() }
                components::Muted { text: format!("{candidates} candidates"), tone: "".to_string() }
                components::Muted { text: format!("{filtered} filtered"), tone: "muted".to_string() }
            }
        }
        for row in rows {
            article { class: "item",
                div { class: "row",
                    input {
                        r#type: "checkbox",
                        checked: row.checked,
                        onchange: move |_| ui.model.write().ai.toggle(row.index),
                    }
                    span { class: "grow", "{row.filename}" }
                    components::Badge { text: row.status, tone: row.tone.to_string() }
                }
                components::Muted { text: row.meta, tone: "".to_string() }
                components::Muted { text: row.url, tone: "".to_string() }
                button {
                    class: "ghost",
                    onclick: move |_| {
                        let mut model = ui.model.write();
                        if !model.ai.expanded.remove(&row.index) {
                            model.ai.expanded.insert(row.index);
                        }
                    },
                    if row.open { "Hide evidence" } else { "View evidence" }
                }
                if row.open {
                    div { class: "evidence",
                        for item in row.evidence {
                            components::Muted { text: item, tone: "".to_string() }
                        }
                        for item in row.missing {
                            components::Muted { text: item, tone: "warning".to_string() }
                        }
                    }
                }
            }
        }
        if empty {
            components::EmptyState {
                title: "No analysis yet".to_string(),
                subtitle: "Paste URLs or reuse the last Discover scan, then choose Find files.".to_string(),
            }
        }
        div { class: "row",
            button {
                class: "ghost",
                onclick: move |_| ui.model.write().ai.set_all(true),
                "Select all"
            }
            components::Muted {
                text: (if incomplete {
                    "Some selected files belong to incomplete archive groups."
                } else {
                    "Selections are advisory only."
                }).to_string(),
                tone: (if incomplete { "warning" } else { "" }).to_string(),
            }
            span { class: "grow" }
            button {
                class: "primary",
                disabled: selected == 0,
                onclick: move |_| {
                    let model = ui.model.write();
                    let candidates = model.ai.selected_for_planner();
                    let destination = model.destination(model.ai_destination.as_ref());
                    model.send(Command::EnqueueAiCandidates { candidates, destination });
                },
                "Add {selected} selected"
            }
        }
    }
}

fn find_files(mut ui: crate::app::Ui) {
    let mut model = ui.model.write();
    model.ai.analyzing = true;
    let probe = model.ai.probe_metadata;
    if model.ai.analyze_discovered {
        let candidates = model.discovery.candidates_for_planner();
        model.send(Command::AnalyzeCandidates { candidates, probe });
    } else {
        let text = model.ai.urls_text.clone();
        model.ai.urls_text = text.clone();
        model.send(Command::AnalyzeUrls { text, probe });
    }
}

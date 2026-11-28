//! Dashboard: a quiet summary of what is transferring and what needs attention.

use dioxus::prelude::*;

use crate::app::{components, format, theme, use_ui};
use crate::models::download_status::DownloadState;

struct TransferRow {
    name: String,
    state: String,
    tone: &'static str,
    fraction: Option<f32>,
    bytes: String,
    speed: String,
}

struct RecentRow {
    name: String,
    state: String,
    tone: &'static str,
}

#[component]
pub fn Dashboard() -> Element {
    let mut ui = use_ui();

    let (stats, active, active_empty, failed, recent, recent_empty) = {
        let model = ui.model.read();
        let stats = model.dashboard.stats;
        let active: Vec<TransferRow> = model
            .queue
            .records()
            .iter()
            .filter(|record| theme::is_active(record.state))
            .map(|record| {
                let view = model.queue.view_for(record);
                TransferRow {
                    name: record.display_name.clone(),
                    state: theme::state_label(record.state).to_string(),
                    tone: theme::state_class(record.state),
                    fraction: view.fraction,
                    bytes: format::format_bytes(Some(view.downloaded_bytes)),
                    speed: speed_label(view.speed_bps),
                }
            })
            .collect();
        let failed: Vec<(String, String)> = model
            .queue
            .records()
            .iter()
            .filter(|record| record.state == DownloadState::Failed)
            .take(5)
            .map(|record| {
                (
                    record.display_name.clone(),
                    record
                        .error_summary
                        .clone()
                        .unwrap_or_else(|| "Download failed".into()),
                )
            })
            .collect();
        let recent: Vec<RecentRow> = model
            .dashboard
            .recent
            .iter()
            .take(5)
            .map(|record| RecentRow {
                name: record.display_name.clone(),
                state: theme::state_label(record.state).to_string(),
                tone: theme::state_class(record.state),
            })
            .collect();
        let active_empty = active.is_empty();
        let recent_empty = recent.is_empty();
        (stats, active, active_empty, failed, recent, recent_empty)
    };

    rsx! {
        components::PageHeader {
            title: "Dashboard".to_string(),
            subtitle: "A live view of transfers, failures, and recent activity.".to_string(),
        }
        div { class: "grid-stats",
            div { class: "stat",
                div { class: "stat-value accent", "{stats.active}" }
                components::Muted { text: "Active".to_string(), tone: "".to_string() }
            }
            div { class: "stat",
                div { class: "stat-value", "{stats.queued}" }
                components::Muted { text: "Queued".to_string(), tone: "".to_string() }
            }
            div { class: "stat",
                div { class: "stat-value success", "{stats.completed}" }
                components::Muted { text: "Completed".to_string(), tone: "".to_string() }
            }
            div { class: "stat",
                div { class: "stat-value danger", "{stats.failed}" }
                components::Muted { text: "Failed".to_string(), tone: "".to_string() }
            }
        }
        section { class: "column",
            components::SectionTitle { text: "Active transfers".to_string(), tone: "".to_string() }
            for row in active {
                article { class: "item",
                    div { class: "row between",
                        span { "{row.name}" }
                        components::Badge { text: row.state, tone: row.tone.to_string() }
                    }
                    components::ProgressBar { fraction: row.fraction }
                    div { class: "row between muted",
                        span { "{row.bytes}" }
                        span { "{row.speed}" }
                    }
                }
            }
            if active_empty {
                components::EmptyState {
                    title: "No active transfers".to_string(),
                    subtitle: "Add a URL to start a download.".to_string(),
                }
            }
        }
        if !failed.is_empty() {
            section { class: "column",
                components::SectionTitle {
                    text: "Needs attention".to_string(),
                    tone: "danger".to_string(),
                }
                for (name, reason) in failed {
                    article { class: "item",
                        div { "{name}" }
                        components::Muted { text: reason, tone: "danger".to_string() }
                    }
                }
            }
        }
        section { class: "column",
            components::SectionTitle { text: "Recent activity".to_string(), tone: "".to_string() }
            for row in recent {
                div { class: "row between",
                    span { "{row.name}" }
                    components::Badge { text: row.state, tone: row.tone.to_string() }
                }
            }
            if recent_empty {
                components::Muted { text: "No history yet.".to_string(), tone: "".to_string() }
            }
        }
        button {
            onclick: move |_| ui.model.write().request_dashboard(),
            "Refresh"
        }
    }
}

fn speed_label(speed: f64) -> String {
    if speed <= 0.0 {
        "—".to_string()
    } else {
        format::format_speed(Some(speed))
    }
}

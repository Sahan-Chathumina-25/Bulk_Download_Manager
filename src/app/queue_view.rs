//! Queue: live rows, per-row controls and a details panel.

use std::path::Path;

use egui::{RichText, Ui};

use crate::app::backend::{Command, QueueAction};
use crate::app::state::{Confirmation, Severity, is_active};
use crate::app::{BulkDownloadApp, components, format, theme};
use crate::models::diagnostic::{Diagnostic, FailureCategory, host_from_url};
use crate::models::download_item::DownloadRecord;
use crate::models::download_status::DownloadState;
use crate::services::validation::redact_url;

/// Legacy copy for an action that needs a selection.
fn no_selection(action: &str) -> String {
    format!("Select one or more queue rows first to {action}.")
}

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(ui, "Queue", "Live downloads, oldest first.");

    ui.horizontal(|ui| {
        if components::primary_button(ui, "Start all").clicked() {
            app.send(Command::StartAll);
        }

        let selected = app.queue.selected_ids();
        if components::secondary_button(ui, "Pause").clicked() {
            if selected.is_empty() {
                app.toast(Severity::Info, no_selection("pause"));
            } else {
                app.send(Command::QueueAction {
                    ids: selected,
                    action: QueueAction::Pause,
                });
            }
        }
        let selected = app.queue.selected_ids();
        if components::secondary_button(ui, "Resume").clicked() {
            if selected.is_empty() {
                app.toast(Severity::Info, no_selection("resume"));
            } else {
                app.send(Command::QueueAction {
                    ids: selected,
                    action: QueueAction::Resume,
                });
            }
        }
        let selected = app.queue.selected_ids();
        if components::secondary_button(ui, "Retry").clicked() {
            let failed: Vec<i64> = app
                .queue
                .records()
                .iter()
                .filter(|record| {
                    record.state == DownloadState::Failed
                        && record.id.is_some_and(|id| selected.contains(&id))
                })
                .filter_map(|record| record.id)
                .collect();
            if selected.is_empty() {
                app.toast(Severity::Info, no_selection("retry"));
            } else if failed.len() != selected.len() {
                app.toast(
                    Severity::Info,
                    "Retry applies to failed downloads only. Select FAILED rows and try again.",
                );
            } else {
                app.send(Command::QueueAction {
                    ids: failed,
                    action: QueueAction::Retry,
                });
            }
        }
        let selected = app.queue.selected_ids();
        if components::danger_button(ui, "Cancel…").clicked() {
            if selected.is_empty() {
                app.toast(Severity::Info, no_selection("cancel"));
            } else {
                app.confirm(Confirmation::CancelQueue { ids: selected });
            }
        }
        let selected = app.queue.selected_ids();
        if components::danger_button(ui, "Remove…").clicked() {
            if selected.is_empty() {
                app.toast(Severity::Info, no_selection("remove"));
            } else {
                app.confirm(Confirmation::RemoveQueue { ids: selected });
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            components::muted(ui, format!("{} row(s)", app.queue.len()));
        });
    });

    ui.add_space(8.0);

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());

        if app.queue.is_empty() {
            components::empty_state(
                ui,
                "Nothing queued yet. Add downloads from the Add Downloads page,\nor scan a web page under Discover.",
            );
            return;
        }

        // Clone the rows so the checkbox handlers may mutate the state while
        // the table is drawn.
        let records: Vec<DownloadRecord> = app.queue.records().to_vec();

        egui::Grid::new("queue_rows")
            .num_columns(6)
            .spacing([14.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                for header in ["", "Name", "State", "Progress", "Speed", "ETA"] {
                    ui.label(RichText::new(header).color(theme::MUTED).size(12.0).strong());
                }
                ui.end_row();

                for record in &records {
                    let id = record.id.unwrap_or(-1);
                    let mut ticked = id >= 0 && app.queue.selected().contains(&id);
                    ui.add_enabled_ui(id >= 0, |ui| {
                        if ui.checkbox(&mut ticked, "").changed() && id >= 0 {
                            app.queue.toggle_selected(id);
                        }
                    });

                    let name = ui.label(RichText::new(&record.display_name).color(theme::TEXT));
                    name.on_hover_text(redact_url(&record.source_url));

                    components::badge(
                        ui,
                        theme::state_label(record.state),
                        theme::state_color(record.state),
                    );

                    let view = app.queue.view_for(record);
                    ui.vertical(|ui| {
                        components::progress_bar(
                            ui,
                            view.fraction,
                            theme::state_color(record.state),
                            200.0,
                        );
                        let total = match view.total_bytes {
                            Some(total) => format::format_bytes(Some(total)),
                            None => format::DASH.to_string(),
                        };
                        components::muted(
                            ui,
                            format!(
                                "{} / {}",
                                format::format_bytes(Some(view.downloaded_bytes)),
                                total
                            ),
                        );
                    });

                    let speed = if is_active(record.state) {
                        format::format_speed(Some(view.speed_bps))
                    } else {
                        format::DASH.to_string()
                    };
                    ui.label(RichText::new(speed).color(theme::MUTED).size(12.0));
                    ui.label(
                        RichText::new(format::format_eta(view.eta_seconds))
                            .color(theme::MUTED)
                            .size(12.0),
                    );
                    ui.end_row();
                }
            });
    });

    ui.add_space(8.0);
    details_panel(app, ui);
}

fn details_panel(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let selected = app.queue.selected_ids();
    if selected.len() != 1 {
        components::card().show(ui, |ui| {
            components::section_title(ui, "Details");
            components::muted(ui, "Select a single queue row to inspect it.");
        });
        return;
    }

    let Some(record) = app
        .queue
        .records()
        .iter()
        .find(|record| record.id == Some(selected[0]))
        .cloned()
    else {
        return;
    };

    let view = app.queue.view_for(&record);
    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_title(ui, "Details");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if components::link_button(ui, "Close").clicked() {
                    app.queue.clear_selection();
                }
                if !record.destination_path.is_empty()
                    && components::link_button(ui, "Open folder").clicked()
                {
                    open_parent_folder(&record.destination_path);
                }
            });
        });
        ui.add_space(6.0);

        egui::Grid::new("queue_details")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                components::detail_row(ui, "Name", &record.display_name);
                components::detail_row(ui, "Source URL", &redact_url(&record.source_url));
                components::detail_row(ui, "Destination", &record.destination_path);
                components::detail_row(ui, "State", theme::state_label(record.state));
                components::detail_row(
                    ui,
                    "Progress",
                    &format!(
                        "{} / {}",
                        format::format_bytes(Some(view.downloaded_bytes)),
                        format::format_bytes(view.total_bytes)
                    ),
                );
                components::detail_row(ui, "Speed", &format::format_speed(Some(view.speed_bps)));
                components::detail_row(ui, "ETA", &format::format_eta(view.eta_seconds));
                components::detail_row(ui, "Created", &record.created_at);
                components::detail_row(
                    ui,
                    "Completed",
                    record.completed_at.as_deref().unwrap_or(format::DASH),
                );
                components::detail_row(ui, "Retries", &record.retry_count.to_string());
                components::detail_row(ui, "ETag", record.etag.as_deref().unwrap_or(format::DASH));
                components::detail_row(
                    ui,
                    "Last modified",
                    record.last_modified.as_deref().unwrap_or(format::DASH),
                );
                components::detail_row(
                    ui,
                    "Checksum",
                    record.checksum_sha256.as_deref().unwrap_or(format::DASH),
                );
                components::detail_row(
                    ui,
                    "Checksum verified",
                    if record.checksum_verified {
                        "Yes"
                    } else {
                        "No"
                    },
                );
                components::detail_row(
                    ui,
                    "Content type",
                    record.content_type.as_deref().unwrap_or(format::DASH),
                );
                components::detail_row(
                    ui,
                    "Error",
                    record.error_summary.as_deref().unwrap_or(format::DASH),
                );

                if let Some(category) = record
                    .failure_category
                    .as_deref()
                    .and_then(FailureCategory::parse)
                {
                    // Only persisted columns are available here; status,
                    // redirect count and elapsed time live in the engine's log
                    // line, so they stay unknown rather than guessed.
                    let diagnostic = Diagnostic {
                        download_id: record.id,
                        host: host_from_url(&record.source_url),
                        redirect_count: 0,
                        status: None,
                        content_type: record.content_type.clone(),
                        category,
                        retries: record.retry_count,
                        elapsed_seconds: 0.0,
                    };
                    ui.label(RichText::new("Diagnostics").color(theme::MUTED).size(12.0));
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(category.headline())
                                .color(category_color(category))
                                .size(12.0)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(diagnostic.summary_line())
                                .color(theme::MUTED)
                                .size(11.0)
                                .monospace(),
                        );
                    });
                    ui.end_row();
                }
            });
    });
}

/// Severity colour for a failure headline.
fn category_color(category: FailureCategory) -> egui::Color32 {
    match category {
        FailureCategory::Cancelled => theme::MUTED,
        FailureCategory::Collision
        | FailureCategory::Timeout
        | FailureCategory::ServerError
        | FailureCategory::Network => theme::WARN,
        _ => theme::ERROR,
    }
}

/// Show the file's folder in the platform file manager.
///
/// This launches the documented OS opener rather than shelling out to a
/// command pipeline; a native ShellExecute binding would require pulling in
/// the `windows` crate, which is outside this phase's dependency budget.
fn open_parent_folder(destination_path: &str) {
    let path = Path::new(destination_path);
    let folder = path.parent().unwrap_or(path);

    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");

    #[cfg(any(target_os = "windows", target_os = "macos", unix))]
    if let Err(error) = command.arg(folder).spawn() {
        tracing::warn!(error = %error, "could not open the download folder");
    }
}

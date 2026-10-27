//! Queue: live rows, per-row controls and a details panel.

use std::path::Path;

use egui::{RichText, Ui};

use crate::app::backend::{Command, QueueAction};
use crate::app::state::{Confirmation, Page, ProgressView, Severity, is_active};
use crate::app::theme::{self, Action, Palette};
use crate::app::{BulkDownloadApp, components, format};
use crate::models::diagnostic::{FailureCategory, host_from_url};
use crate::models::download_item::DownloadRecord;
use crate::models::download_status::DownloadState;
use crate::services::validation::redact_url;

/// One per-row control the page can issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Start,
    Pause,
    Retry,
    Cancel,
    Remove,
}

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    components::page_header(
        ui,
        "Queue",
        "Every download, its state, and its controls",
        None,
    );

    ui.horizontal(|ui| {
        if components::button(ui, "Start all", Action::Start).clicked() {
            app.send(Command::StartAll);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            components::muted(
                ui,
                format!(
                    "{} row(s) · {} selected",
                    app.queue.len(),
                    app.queue.selected_ids().len()
                ),
            );
        });
    });

    ui.add_space(theme::SPACE_2);
    bulk_toolbar(app, ui);
    ui.add_space(theme::SPACE_2);
    rows_card(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    details_panel(app, ui);
}

/// Bulk actions that operate on the current tick selection.
///
/// Every control is enabled only for the states it can actually act on, so a
/// bulk click can never turn into a no-op or an error.
fn bulk_toolbar(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());

        let selected = app.queue.selected_ids();
        let has_selection = !selected.is_empty();
        let mut can_pause = false;
        let mut can_resume = false;
        let mut failed: Vec<i64> = Vec::new();
        for record in app.queue.records() {
            let Some(id) = record.id else {
                continue;
            };
            if !selected.contains(&id) {
                continue;
            }
            match record.state {
                DownloadState::Connecting | DownloadState::Downloading => can_pause = true,
                DownloadState::Queued | DownloadState::Paused | DownloadState::Interrupted => {
                    can_resume = true;
                }
                DownloadState::Failed => failed.push(id),
                DownloadState::Completed | DownloadState::Cancelled => {}
            }
        }
        // Retry is only truthful when every selected row is a failed one.
        let can_retry = !failed.is_empty() && failed.len() == selected.len();

        ui.horizontal_wrapped(|ui| {
            components::section_heading(ui, "Selected");
            let selected_count = selected.len();
            components::status_badge(
                ui,
                &format!("{selected_count} selected"),
                if selected_count == 0 {
                    palette.muted
                } else {
                    palette.accent
                },
            );

            ui.add_enabled_ui(can_pause, |ui| {
                if components::button(ui, "Pause", Action::Pause).clicked() {
                    app.send(Command::QueueAction {
                        ids: selected.clone(),
                        action: QueueAction::Pause,
                    });
                }
            });

            ui.add_enabled_ui(can_resume, |ui| {
                if components::button(ui, "Resume", Action::Start).clicked() {
                    app.send(Command::QueueAction {
                        ids: selected.clone(),
                        action: QueueAction::Resume,
                    });
                }
            });

            ui.add_enabled_ui(can_retry, |ui| {
                if components::button(ui, "Retry", Action::Retry).clicked() {
                    app.send(Command::QueueAction {
                        ids: failed.clone(),
                        action: QueueAction::Retry,
                    });
                }
            });

            ui.add_enabled_ui(has_selection, |ui| {
                if components::button(ui, "Cancel…", Action::Cancel).clicked() {
                    app.confirm(Confirmation::CancelQueue {
                        ids: selected.clone(),
                    });
                }
            });

            ui.add_enabled_ui(has_selection, |ui| {
                if components::button(ui, "Remove…", Action::Failed).clicked() {
                    app.confirm(Confirmation::RemoveQueue {
                        ids: selected.clone(),
                    });
                }
            });
        });

        if !has_selection {
            components::muted(
                ui,
                "Tick a row for bulk actions. The buttons on each row work without a selection.",
            );
        } else if !can_pause && !can_resume && failed.is_empty() {
            components::muted(
                ui,
                "The selected rows are already settled; use a per-row control or remove them.",
            );
        }
    });
}

/// The compact row list.
fn rows_card(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    let records = app.queue.records().to_vec();

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_heading(ui, "Queue");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                components::muted(ui, format!("{} row(s)", records.len()));
            });
        });

        if app.queue.is_empty() {
            if components::empty_state_action(
                ui,
                "Nothing queued yet",
                "Add URLs on Add Downloads, or scan a page under Discover.",
                "Add Downloads",
            ) {
                app.navigate_to(Page::AddDownloads);
            }
            return;
        }

        ui.add_space(theme::SPACE_1);
        for record in &records {
            queue_row(app, ui, record, palette);
            ui.add_space(theme::SPACE_1);
        }
    });
}

/// One queue row with truthful progress and state-appropriate controls.
fn queue_row(app: &mut BulkDownloadApp, ui: &mut Ui, record: &DownloadRecord, palette: Palette) {
    let state = record.state;
    let id = record.id;
    let ticked = id.is_some_and(|id| app.queue.selected().contains(&id));
    let view = app.queue.view_for(record);
    let color = palette.state_color(state);
    let host = host_from_url(&record.source_url);
    let show_bar = view.fraction.is_some() || is_active(state);
    let mut action: Option<RowAction> = None;

    components::download_row(ui, ticked, |ui| {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(id.is_some(), |ui| {
                let mut checked = ticked;
                if ui.checkbox(&mut checked, "").changed()
                    && let Some(id) = id
                {
                    app.queue.toggle_selected(id);
                }
            });

            // Trailing controls first, so the name gets exactly the space that
            // is left and truncates instead of pushing the row past the card.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                components::status_badge(ui, theme::state_label(state), color);
                row_actions(ui, record, palette, &mut action);

                // Name and metadata read from the left edge; only the state
                // badge and controls are pinned to the right.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.vertical(|ui| {
                        components::ellipsized_label(
                            ui,
                            RichText::new(&record.display_name)
                                .size(theme::BODY_SIZE)
                                .color(palette.text),
                            &redact_url(&record.source_url),
                        );

                        ui.horizontal_wrapped(|ui| {
                            if !host.is_empty() {
                                components::muted(ui, host.clone());
                            }
                            components::muted(ui, size_text(&view));
                            if is_active(state) && view.speed_bps > 0.0 {
                                components::muted(
                                    ui,
                                    format!(
                                        "· {} · ETA {}",
                                        format::format_speed(Some(view.speed_bps)),
                                        format::format_eta(view.eta_seconds)
                                    ),
                                );
                            }
                        });
                    });
                });
            });
        });

        if show_bar {
            components::progress_bar(ui, view.fraction, color, ui.available_width());
        } else if state == DownloadState::Queued {
            components::muted(ui, "Waiting for a free slot.");
        }

        if let Some(error) = record.error_summary.as_deref()
            && matches!(state, DownloadState::Failed | DownloadState::Cancelled)
        {
            components::ellipsized_label(
                ui,
                RichText::new(error)
                    .size(theme::SMALL_SIZE)
                    .color(palette.error),
                error,
            );
        }
    });

    if let Some(action) = action {
        apply_row_action(app, record, action);
    }
}

/// Downloaded / total size for one row, keeping unknown totals explicit.
fn size_text(view: &ProgressView) -> String {
    let total = view
        .total_bytes
        .map(|total| format::format_bytes(Some(total)))
        .unwrap_or_else(|| "unknown total".to_string());
    format!(
        "{} / {}",
        format::format_bytes(Some(view.downloaded_bytes)),
        total
    )
}

/// Icons valid for the row's current state. Every icon carries a tooltip.
fn row_actions(
    ui: &mut Ui,
    record: &DownloadRecord,
    palette: Palette,
    action: &mut Option<RowAction>,
) {
    match record.state {
        DownloadState::Queued => {
            if components::icon_button_colored(ui, "▶", "Start this download", palette.success)
                .clicked()
            {
                *action = Some(RowAction::Start);
            }
        }
        DownloadState::Connecting | DownloadState::Downloading => {
            if components::icon_button_colored(ui, "‖", "Pause this download", palette.warning)
                .clicked()
            {
                *action = Some(RowAction::Pause);
            }
            if components::icon_button_colored(ui, "✕", "Cancel this download", palette.error)
                .clicked()
            {
                *action = Some(RowAction::Cancel);
            }
        }
        DownloadState::Paused | DownloadState::Interrupted => {
            if components::icon_button_colored(ui, "▶", "Resume this download", palette.success)
                .clicked()
            {
                *action = Some(RowAction::Start);
            }
            if components::icon_button_colored(ui, "✕", "Cancel this download", palette.error)
                .clicked()
            {
                *action = Some(RowAction::Cancel);
            }
        }
        DownloadState::Failed | DownloadState::Cancelled => {
            if components::icon_button_colored(ui, "↻", "Retry this download", palette.violet)
                .clicked()
            {
                *action = Some(RowAction::Retry);
            }
            if components::icon_button_colored(ui, "⌫", "Remove this row", palette.muted).clicked()
            {
                *action = Some(RowAction::Remove);
            }
        }
        DownloadState::Completed => {
            if components::icon_button_colored(ui, "⌫", "Remove this row", palette.muted).clicked()
            {
                *action = Some(RowAction::Remove);
            }
        }
    }

    if file_exists(record)
        && components::icon_button_colored(ui, "⌂", "Open containing folder", palette.info)
            .clicked()
    {
        open_parent_folder(&record.destination_path);
    }
}

fn file_exists(record: &DownloadRecord) -> bool {
    let path = record.destination_path.trim();
    !path.is_empty() && Path::new(path).exists()
}

/// Route a row control through the existing commands/confirmation flow.
fn apply_row_action(app: &mut BulkDownloadApp, record: &DownloadRecord, action: RowAction) {
    let Some(id) = record.id else {
        app.toast(
            Severity::Info,
            "This row is still being registered; try again in a moment.",
        );
        return;
    };

    match action {
        RowAction::Start => app.send(Command::QueueAction {
            ids: vec![id],
            action: QueueAction::Resume,
        }),
        RowAction::Pause => app.send(Command::QueueAction {
            ids: vec![id],
            action: QueueAction::Pause,
        }),
        RowAction::Retry => app.send(Command::QueueAction {
            ids: vec![id],
            action: QueueAction::Retry,
        }),
        RowAction::Cancel => app.confirm(Confirmation::CancelQueue { ids: vec![id] }),
        RowAction::Remove => app.confirm(Confirmation::RemoveQueue { ids: vec![id] }),
    }
}

fn details_panel(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let selected = app.queue.selected_ids();
    if selected.len() != 1 {
        components::card_for(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_heading(ui, "Details");
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

    let palette = theme::palette(ui);
    let view = app.queue.view_for(&record);

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_heading(ui, "Details");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if components::link_button(ui, "Close").clicked() {
                    app.queue.clear_selection();
                }
                if file_exists(&record)
                    && components::icon_button_colored(
                        ui,
                        "⌂",
                        "Open containing folder",
                        palette.info,
                    )
                    .clicked()
                {
                    open_parent_folder(&record.destination_path);
                }
            });
        });
        ui.add_space(theme::SPACE_1);

        let host = host_from_url(&record.source_url);
        egui::Grid::new("queue_details")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                components::detail_row(ui, "Name", &record.display_name);
                components::detail_row(ui, "Host", if host.is_empty() { "unknown" } else { &host });
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
            });

        if let Some(category) = record
            .failure_category
            .as_deref()
            .and_then(FailureCategory::parse)
        {
            ui.add_space(theme::SPACE_2);
            egui::CollapsingHeader::new(
                RichText::new("Diagnostics")
                    .size(theme::BODY_SIZE)
                    .color(palette.text_secondary),
            )
            .default_open(true)
            .show(ui, |ui| {
                let retries = record.retry_count.to_string();
                let content_type = record
                    .content_type
                    .as_deref()
                    .unwrap_or(format::DASH)
                    .to_string();
                let details = [
                    ("Host", if host.is_empty() { "unknown" } else { &host }),
                    ("Category", category.as_str()),
                    ("Retries", retries.as_str()),
                    ("Content type", content_type.as_str()),
                ];
                components::diagnostic_panel(
                    ui,
                    category.headline(),
                    category_color(category, palette),
                    &details,
                );
            });
        }
    });
}

/// Severity colour for a failure headline in the active palette.
fn category_color(category: FailureCategory, palette: Palette) -> egui::Color32 {
    match category {
        FailureCategory::Cancelled => palette.muted,
        FailureCategory::Collision
        | FailureCategory::Timeout
        | FailureCategory::ServerError
        | FailureCategory::Network => palette.warning,
        _ => palette.error,
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

//! Dashboard: what is happening now, what needs attention, and what next.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::{Page, is_active};
use crate::app::theme::Action;
use crate::app::{BulkDownloadApp, components, format, theme};
use crate::models::diagnostic::FailureCategory;
use crate::models::download_status::DownloadState;
use crate::services::validation::redact_url;

/// How many rows each dashboard section shows before deferring to its page.
const SECTION_LIMIT: usize = 5;

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    if components::page_header(
        ui,
        "Dashboard",
        "Live overview of everything in progress",
        Some("Add Downloads"),
    ) {
        app.navigate_to(Page::AddDownloads);
    }

    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if components::button(ui, "Refresh", Action::Secondary).clicked() {
                app.send(Command::RefreshDashboard);
            }
        });
    });

    if let Some(error) = &app.dashboard.error {
        components::status_badge(ui, error, palette.error);
        ui.add_space(theme::SPACE_2);
    }

    let stats = app.dashboard.stats;
    ui.horizontal_wrapped(|ui| {
        components::stat_tile(
            ui,
            "Active",
            stats.active.to_string(),
            palette.accent,
            Some(|| app.navigate_to(Page::Queue)),
        );
        components::stat_tile(
            ui,
            "Queued",
            stats.queued.to_string(),
            palette.text_secondary,
            Some(|| app.navigate_to(Page::Queue)),
        );
        components::stat_tile(
            ui,
            "Completed",
            stats.completed.to_string(),
            palette.success,
            Some(|| {
                app.history.set_state_filter(Some(DownloadState::Completed));
                app.navigate_to(Page::History);
            }),
        );
        components::stat_tile(
            ui,
            "Failed",
            stats.failed.to_string(),
            palette.error,
            Some(|| {
                app.history.set_state_filter(Some(DownloadState::Failed));
                app.navigate_to(Page::History);
            }),
        );
    });

    ui.add_space(theme::SPACE_3);

    let nothing_yet = stats.active == 0
        && stats.queued == 0
        && stats.completed == 0
        && stats.failed == 0
        && app.dashboard.recent.is_empty();

    if nothing_yet {
        components::card_for(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if components::empty_state_action(
                ui,
                "Nothing downloading yet",
                "Add a few URLs or import a text file to get started.",
                "Add Downloads",
            ) {
                app.navigate_to(Page::AddDownloads);
            }
        });
        return;
    }

    active_section(app, ui);
    ui.add_space(theme::SPACE_2);
    attention_section(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    recent_section(app, ui);
}

/// Live rows with their truthful progress snapshots.
fn active_section(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let active: Vec<_> = app
        .queue
        .records()
        .iter()
        .filter(|record| is_active(record.state))
        .cloned()
        .collect();

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Active downloads");
        if active.is_empty() {
            ui.add_space(theme::SPACE_1);
            components::muted(ui, "No downloads are transferring right now.");
            return;
        }

        ui.add_space(theme::SPACE_1);
        for record in &active {
            let view = app.queue.view_for(record);
            let palette = theme::palette(ui);
            components::download_row(ui, false, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        components::status_badge(
                            ui,
                            theme::state_label(record.state),
                            palette.state_color(record.state),
                        );
                        // The name takes the remaining width from the left so it
                        // reads like a row title, not a right-aligned caption.
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            components::ellipsized_label(
                                ui,
                                RichText::new(&record.display_name)
                                    .size(theme::BODY_SIZE)
                                    .color(palette.text),
                                &redact_url(&record.source_url),
                            );
                        });
                    });
                });
                components::progress_bar(
                    ui,
                    view.fraction,
                    palette.state_color(record.state),
                    ui.available_width(),
                );
                let total = match view.total_bytes {
                    Some(total) => format::format_bytes(Some(total)),
                    None => format::DASH.to_string(),
                };
                ui.horizontal(|ui| {
                    components::muted(
                        ui,
                        format!(
                            "{} / {} · {} · ETA {}",
                            format::format_bytes(Some(view.downloaded_bytes)),
                            total,
                            format::format_speed(Some(view.speed_bps)),
                            format::format_eta(view.eta_seconds),
                        ),
                    );
                });
            });
            ui.add_space(theme::SPACE_1);
        }
    });
}

/// Failed rows that still need a decision, with their category label.
fn attention_section(app: &mut BulkDownloadApp, ui: &mut Ui, palette: theme::Palette) {
    let stats = app.dashboard.stats;
    if stats.failed == 0 {
        return;
    }

    let failed: Vec<_> = app
        .dashboard
        .recent
        .iter()
        .filter(|record| record.state == DownloadState::Failed)
        .take(SECTION_LIMIT)
        .cloned()
        .collect();

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_heading(ui, "Needs attention");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                components::status_badge(ui, &format!("{} failed", stats.failed), palette.error);
            });
        });

        if failed.is_empty() {
            ui.add_space(theme::SPACE_1);
            components::muted(
                ui,
                "Failed downloads are recorded in History. Open the queue to retry them.",
            );
        } else {
            ui.add_space(theme::SPACE_1);
            for record in &failed {
                components::download_row(ui, false, |ui| {
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            components::status_badge(ui, "Failed", palette.error);
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    components::ellipsized_label(
                                        ui,
                                        RichText::new(&record.display_name)
                                            .size(theme::BODY_SIZE)
                                            .color(palette.text),
                                        &redact_url(&record.source_url),
                                    );
                                },
                            );
                        });
                    });

                    let category = record
                        .failure_category
                        .as_deref()
                        .and_then(FailureCategory::parse);
                    ui.horizontal(|ui| {
                        if let Some(category) = category {
                            ui.label(
                                RichText::new(category.headline())
                                    .size(theme::SMALL_SIZE)
                                    .color(palette.warning),
                            );
                            ui.add_space(theme::SPACE_2);
                        }
                        if let Some(summary) = &record.error_summary {
                            components::ellipsized_label(
                                ui,
                                RichText::new(summary)
                                    .size(theme::SMALL_SIZE)
                                    .color(palette.muted),
                                summary,
                            );
                        }
                    });
                });
                ui.add_space(theme::SPACE_1);
            }
            if failed.len() < stats.failed {
                components::muted(
                    ui,
                    format!(
                        "Showing the {} most recent of {} failed downloads.",
                        failed.len(),
                        stats.failed
                    ),
                );
            }
        }

        ui.add_space(theme::SPACE_2);
        if components::button(ui, "Open queue", Action::Info).clicked() {
            app.navigate_to(Page::Queue);
        }
    });
}

/// The dashboard's newest rows, including terminal states.
fn recent_section(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_heading(ui, "Recent activity");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if components::link_button(ui, "View history").clicked() {
                    app.navigate_to(Page::History);
                }
            });
        });

        if app.dashboard.recent.is_empty() {
            ui.add_space(theme::SPACE_1);
            components::muted(ui, "No activity yet.");
            return;
        }

        ui.add_space(theme::SPACE_1);
        let palette = theme::palette(ui);
        for record in app.dashboard.recent.iter().take(SECTION_LIMIT + 1) {
            components::download_row(ui, false, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format::short_timestamp(&record.created_at))
                                .size(theme::SMALL_SIZE)
                                .color(palette.muted),
                        );
                        ui.label(
                            RichText::new(format::format_bytes(record.expected_size))
                                .size(theme::SMALL_SIZE)
                                .color(palette.muted),
                        );
                        components::status_badge(
                            ui,
                            theme::state_label(record.state),
                            palette.state_color(record.state),
                        );
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            components::ellipsized_label(
                                ui,
                                RichText::new(&record.display_name)
                                    .size(theme::BODY_SIZE)
                                    .color(palette.text),
                                &redact_url(&record.source_url),
                            );
                        });
                    });
                });
            });
            ui.add_space(theme::SPACE_1);
        }
    });
}

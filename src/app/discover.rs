//! Discover: scan a web page for downloadable links and queue the ticked rows.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::Severity;
use crate::app::{BulkDownloadApp, components, format, theme};
use crate::services::validation::redact_url;

/// Legacy copy for a scan that produced no candidates.
const NO_LINKS: &str = "No downloadable links found on that page. Try another page, or relax Settings → Include unknown types.";
/// Same wording as the Add Downloads destination warning.
const NO_DESTINATION: &str = "No destination folder set. Set one in Settings → Destination folder.";

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(
        ui,
        "Discover",
        "Scan a page for links, tick the files you want, then queue them.",
    );

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("Page URL").color(theme::MUTED));
            ui.add(
                egui::TextEdit::singleline(&mut app.discovery.page_url)
                    .hint_text("https://example.com/downloads")
                    .desired_width(420.0),
            );

            let can_scan = !app.discovery.page_url.trim().is_empty() && !app.discovery.scanning;
            ui.add_enabled_ui(can_scan, |ui| {
                if components::primary_button(ui, "Scan page").clicked() {
                    app.discovery.scanning = true;
                    app.discovery.status = format!("Scanning {}…", app.discovery.page_url.trim());
                    app.send(Command::ScanPage {
                        url: app.discovery.page_url.trim().to_string(),
                    });
                }
            });

            if app.discovery.scanning {
                if components::danger_button(ui, "Cancel scan").clicked() {
                    app.send(Command::CancelScan);
                }
            } else if components::secondary_button(ui, "Clear results").clicked() {
                app.discovery.clear();
            }
        });

        if !app.discovery.status.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(&app.discovery.status).color(theme::MUTED));
        }
    });

    ui.add_space(8.0);

    let visible = app.discovery.visible_indices();
    let result_errors = app
        .discovery
        .result
        .as_ref()
        .map(|result| result.errors.clone())
        .unwrap_or_default();

    if !result_errors.is_empty() {
        components::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_title(ui, "Scan problems");
            for error in &result_errors {
                ui.label(RichText::new(error).color(theme::WARN).size(12.0));
            }
        });
        ui.add_space(8.0);
    }

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_title(ui, "Links");
            ui.add_space(8.0);
            components::muted(
                ui,
                format!(
                    "{} ticked of {} shown",
                    app.discovery.selected_count(),
                    visible.len()
                ),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut app.discovery.include_unknown, "Include unknown types");
                if components::link_button(ui, "Deselect all").clicked() {
                    app.discovery.set_all(false);
                }
                if components::link_button(ui, "Select all").clicked() {
                    app.discovery.set_all(true);
                }
            });
        });
        ui.add_space(6.0);

        if app.discovery.result.is_none() {
            components::empty_state(ui, "Enter a page URL above and scan it to find links.");
            return;
        }
        if app.discovery.candidates().is_empty() {
            components::empty_state(ui, NO_LINKS);
            return;
        }
        if visible.is_empty() {
            components::empty_state(ui, NO_LINKS);
            return;
        }

        // Clone the visible rows so the checkbox handlers can mutate the
        // discovery state while the table is being drawn.
        let rows: Vec<(usize, crate::models::discovered_link::DiscoveryCandidate)> = visible
            .iter()
            .map(|&index| (index, app.discovery.candidates()[index].clone()))
            .collect();

        egui::Grid::new("discover_rows")
            .num_columns(6)
            .spacing([14.0, 8.0])
            .striped(true)
            .show(ui, |ui| {
                for header in ["", "Name", "Type", "Size", "Content type", "Looks like"] {
                    ui.label(
                        RichText::new(header)
                            .color(theme::MUTED)
                            .size(12.0)
                            .strong(),
                    );
                }
                ui.end_row();

                for (index, candidate) in &rows {
                    let mut ticked = app.discovery.is_selected(*index);
                    if ui.checkbox(&mut ticked, "").changed() {
                        app.discovery.toggle(*index);
                    }

                    let name = ui.label(RichText::new(&candidate.filename).color(theme::TEXT));
                    let tooltip = match candidate.anchor_text.as_deref() {
                        Some(anchor) if !anchor.is_empty() => {
                            format!("{}\n{}", redact_url(&candidate.candidate_url), anchor)
                        }
                        _ => redact_url(&candidate.candidate_url),
                    };
                    name.on_hover_text(tooltip);

                    ui.label(
                        RichText::new(candidate.extension.as_deref().unwrap_or(format::DASH))
                            .color(theme::MUTED),
                    );
                    ui.label(
                        RichText::new(format::format_bytes(candidate.size)).color(theme::MUTED),
                    );
                    ui.label(
                        RichText::new(candidate.content_type.as_deref().unwrap_or(format::DASH))
                            .color(theme::MUTED)
                            .size(12.0),
                    );
                    if let Some(reason) = &candidate.filtered_reason {
                        ui.label(
                            RichText::new("Filtered")
                                .color(theme::WARN)
                                .size(12.0)
                                .strong(),
                        )
                        .on_hover_text(reason);
                    } else if candidate.is_likely_file {
                        components::badge(ui, "File", theme::SUCCESS);
                    } else {
                        components::badge(ui, "Link", theme::MUTED);
                    }
                    ui.end_row();
                }
            });
    });

    ui.add_space(8.0);

    let destination = app
        .discovery_destination
        .clone()
        .or_else(|| app.add.effective_destination(&app.settings));

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_title(ui, "Queue");
            match &destination {
                Some(path) => ui.label(
                    RichText::new(path.display().to_string())
                        .color(theme::TEXT)
                        .monospace(),
                ),
                None => ui.label(RichText::new("No destination").color(theme::WARN)),
            };
            if components::secondary_button(ui, "Browse…").clicked()
                && let Some(folder) = rfd::FileDialog::new().pick_folder()
            {
                app.discovery_destination = Some(folder);
            }
            if app.discovery_destination.is_some()
                && components::link_button(ui, "Use default").clicked()
            {
                app.discovery_destination = None;
            }
        });

        if destination.is_none() {
            components::muted(ui, NO_DESTINATION);
        }
        ui.add_space(6.0);

        let candidates = app.discovery.candidates_for_planner();
        if components::primary_button(ui, "Add Selected to Queue").clicked() {
            if candidates.is_empty() {
                app.toast(Severity::Warning, "Tick at least one link first.");
            } else if let Some(destination) = destination {
                app.send(Command::EnqueueCandidates {
                    candidates,
                    destination,
                });
            } else {
                app.toast(Severity::Warning, NO_DESTINATION);
            }
        }
    });

    if let Some(outcome) = &app.discovery.outcome {
        ui.add_space(8.0);
        components::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_title(ui, "Result");
            ui.label(
                RichText::new(format!(
                    "Queued {} link(s); {} started.",
                    outcome.added, outcome.started
                ))
                .color(if outcome.failed.is_empty() {
                    theme::SUCCESS
                } else {
                    theme::WARN
                }),
            );
            for (url, reason) in outcome.failed.iter().take(8) {
                ui.label(
                    RichText::new(format!("✗ {} — {reason}", redact_url(url)))
                        .color(theme::ERROR)
                        .size(12.0),
                );
            }
            if outcome.failed.len() > 8 {
                components::muted(ui, format!("…and {} more.", outcome.failed.len() - 8));
            }
            if !outcome.failed.is_empty() {
                components::muted(ui, format!("{} failure(s) total.", outcome.failed.len()));
            }
        });
    }
}

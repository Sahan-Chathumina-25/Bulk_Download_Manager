//! Discover: scan a web page for downloadable links and queue the ticked rows.
//!
//! The page never performs I/O itself: it sends the existing scan/enqueue
//! commands and only renders the state the backend reports. A candidate is
//! shown with its real classification; a link that is merely a landing page is
//! never presented as a direct file.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::theme::{self, Action, Palette};
use crate::app::{BulkDownloadApp, components, format};
use crate::models::diagnostic::host_from_url;
use crate::models::discovered_link::DiscoveryCandidate;
use crate::services::validation::redact_url;

/// Shown when a scan produced no candidates at all.
const NO_LINKS: &str =
    "That page did not expose any links. Try another page, or enable unknown types below.";
/// Same wording as the Add Downloads destination warning.
const NO_DESTINATION: &str =
    "No destination folder set. Set one in Settings → Storage & history, or pick one here.";

/// Visual classification for one candidate row.
struct Classification {
    label: &'static str,
    color: egui::Color32,
    explanation: &'static str,
}

fn classify(candidate: &DiscoveryCandidate, palette: Palette) -> Classification {
    if candidate.filtered_reason.is_some() {
        return Classification {
            label: "Filtered",
            color: palette.muted,
            explanation: "Matched the ad/tracker filter and cannot be queued.",
        };
    }
    if candidate
        .content_type
        .as_deref()
        .is_some_and(|content_type| content_type.to_ascii_lowercase().contains("text/html"))
    {
        return Classification {
            label: "Verify",
            color: palette.warning,
            explanation: "The server reports an HTML page; this may be a landing page or a browser check.",
        };
    }
    if candidate.is_likely_file {
        Classification {
            label: "Direct file",
            color: palette.success,
            explanation: "The URL ends in a known downloadable extension.",
        }
    } else {
        Classification {
            label: "Landing page",
            color: palette.text_secondary,
            explanation: "No known file extension. Open it first: it may be a landing page, not a file.",
        }
    }
}

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(
        ui,
        "Discover",
        "Scan a page for links, tick the files you want, then queue them.",
    );

    scan_card(app, ui);
    ui.add_space(theme::SPACE_2);
    candidates_card(app, ui);
    ui.add_space(theme::SPACE_2);
    queue_card(app, ui);

    if app.discovery.outcome.is_some() {
        ui.add_space(theme::SPACE_2);
        outcome_card(app, ui);
    }
}

/// URL field, scan/cancel controls and the scan's live progress line.
fn scan_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Scan a page");
        ui.add_space(theme::SPACE_1);

        let can_scan = !app.discovery.page_url.trim().is_empty() && !app.discovery.scanning;
        let mut scan_clicked = false;

        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut app.discovery.page_url)
                    .hint_text("https://example.com/downloads")
                    .desired_width(420.0),
            );
            let submitted =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if submitted && can_scan {
                scan_clicked = true;
            }

            ui.add_enabled_ui(can_scan, |ui| {
                if components::button(ui, "Scan page", Action::Discover).clicked() {
                    scan_clicked = true;
                }
            });

            if app.discovery.scanning {
                if components::button(ui, "Cancel scan", Action::Cancel).clicked() {
                    app.send(Command::CancelScan);
                }
            } else if components::link_button(ui, "Clear results").clicked() {
                app.discovery.clear();
            }
        });

        if scan_clicked {
            app.discovery.scanning = true;
            app.discovery.status = format!("Scanning {}…", app.discovery.page_url.trim());
            app.send(Command::ScanPage {
                url: app.discovery.page_url.trim().to_string(),
            });
        }

        if app.discovery.scanning {
            ui.add_space(theme::SPACE_1);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(&app.discovery.status)
                        .size(theme::SMALL_SIZE)
                        .color(palette.warning),
                );
            });
        } else if !app.discovery.status.is_empty() {
            ui.add_space(theme::SPACE_1);
            components::muted(ui, app.discovery.status.as_str());
        }
    });
}

/// Candidate summary, filter controls, empty states and the row list.
fn candidates_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    let candidates = app.discovery.candidates();
    let total = candidates.len();
    let likely = candidates
        .iter()
        .filter(|candidate| candidate.is_likely_file)
        .count();
    let filtered = app
        .discovery
        .result
        .as_ref()
        .map_or(0, |result| result.filtered_count);
    let visible = app.discovery.visible_indices();

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            components::section_heading(ui, "Candidates");
            ui.add_space(theme::SPACE_2);
            components::status_badge(
                ui,
                &format!("{total} link(s)"),
                palette.text_secondary,
            );
            components::status_badge(ui, &format!("{likely} direct"), palette.success);
            if filtered > 0 {
                components::status_badge(ui, &format!("{filtered} filtered"), palette.muted);
            }
            components::status_badge(
                ui,
                &format!("{} selected", app.discovery.selected_count()),
                palette.accent,
            );
        });

        ui.add_space(theme::SPACE_1);
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(
                &mut app.discovery.include_unknown,
                "Include unknown types",
            );
            if components::link_button(ui, "Select all").clicked() {
                app.discovery.set_all(true);
            }
            if components::link_button(ui, "Deselect all").clicked() {
                app.discovery.set_all(false);
            }
        });
        ui.add_space(theme::SPACE_2);

        let Some(result) = app.discovery.result.clone() else {
            if app.discovery.scanning {
                components::empty_state_action(
                    ui,
                    "Scanning…",
                    "Fetching the page and classifying every link it exposes.",
                    "",
                );
            } else if app.discovery.status.contains("cancelled") {
                components::empty_state_action(
                    ui,
                    "Scan cancelled",
                    "Nothing was changed. Enter a page URL and scan again when you are ready.",
                    "",
                );
            } else {
                components::empty_state_action(
                    ui,
                    "No scan yet",
                    "Enter a page URL above to discover downloadable links. Nothing is fetched until you press Scan.",
                    "",
                );
            }
            return;
        };

        let source_known = !result.source_url.is_empty();
        let visible_empty = visible.is_empty();
        let all_filtered = !visible_empty
            && visible.iter().all(|index| {
                app.discovery.candidates()[*index]
                    .filtered_reason
                    .is_some()
            });

        if result.candidates.is_empty() {
            if !source_known && !result.errors.is_empty() {
                components::empty_state_action(
                    ui,
                    "Scan failed",
                    "The page could not be fetched. Review the problem below and try again.",
                    "",
                );
                scan_errors(ui, &result.errors, palette.error);
            } else if result.errors.is_empty() {
                components::empty_state_action(ui, "No links found", NO_LINKS, "");
            } else {
                components::empty_state_action(
                    ui,
                    "No downloadable links",
                    "The page loaded but no links were extracted. The notes below explain why.",
                    "",
                );
                scan_errors(ui, &result.errors, palette.warning);
            }
            return;
        }

        if visible_empty {
            components::empty_state_action(
                ui,
                "No links match the filter",
                "Only classified file links are shown. Turn on “Include unknown types” to inspect the rest.",
                "",
            );
            return;
        }

        if all_filtered {
            components::empty_state_action(
                ui,
                "All visible links were filtered",
                &format!(
                    "{} link(s) matched the ad/tracker filter and cannot be queued. Adjust the filter list in Settings → Discovery & filtering.",
                    visible.len()
                ),
                "",
            );
            return;
        }

        if !result.errors.is_empty() {
            scan_errors(ui, &result.errors, palette.warning);
            ui.add_space(theme::SPACE_2);
        }

        // Clone the visible rows so the checkbox handlers can mutate the
        // discovery state while the list is being drawn.
        let rows: Vec<(usize, DiscoveryCandidate)> = visible
            .iter()
            .map(|&index| (index, app.discovery.candidates()[index].clone()))
            .collect();

        for (index, candidate) in &rows {
            let class = classify(candidate, palette);
            let selectable = candidate.filtered_reason.is_none();
            let ticked = selectable && app.discovery.is_selected(*index);

            components::download_row(ui, ticked, |ui| {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(selectable, |ui| {
                        let mut checked = ticked;
                        if ui.checkbox(&mut checked, "").changed() {
                            app.discovery.toggle(*index);
                        }
                    });

                    let name_color = if selectable {
                        palette.text
                    } else {
                        palette.muted
                    };
                    let mut tooltip = redact_url(&candidate.candidate_url);
                    if let Some(anchor) = candidate
                        .anchor_text
                        .as_deref()
                        .filter(|anchor| !anchor.is_empty())
                    {
                        tooltip.push_str(&format!("\nAnchor: {anchor}"));
                    }
                    if !candidate.source_page_url.is_empty() {
                        tooltip.push_str(&format!(
                            "\nFrom: {}",
                            redact_url(&candidate.source_page_url)
                        ));
                    }
                    tooltip.push_str(&format!("\n{}", class.explanation));

                    // Right-hand metadata first so the filename truncates into
                    // the leftover width instead of widening the row.
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new(format::format_bytes(candidate.size))
                                    .size(theme::SMALL_SIZE)
                                    .color(palette.muted),
                            );
                            if let Some(content_type) = &candidate.content_type {
                                ui.label(
                                    RichText::new(content_type)
                                        .size(theme::SMALL_SIZE)
                                        .color(palette.muted),
                                )
                                .on_hover_text(content_type);
                            }

                            ui.vertical(|ui| {
                                components::ellipsized_label(
                                    ui,
                                    RichText::new(&candidate.filename)
                                        .size(theme::BODY_SIZE)
                                        .color(name_color),
                                    &tooltip,
                                );

                                ui.horizontal_wrapped(|ui| {
                                    let host = host_from_url(&candidate.candidate_url);
                                    components::muted(
                                        ui,
                                        if host.is_empty() {
                                            "unknown host".to_string()
                                        } else {
                                            host
                                        },
                                    );
                                    components::status_badge(ui, class.label, class.color);

                                    let extension = candidate
                                        .extension
                                        .as_deref()
                                        .map(str::to_uppercase)
                                        .unwrap_or_else(|| "UNKNOWN".to_string());
                                    components::muted(ui, extension);

                                    if let Some(reason) = &candidate.filtered_reason {
                                        components::ellipsized_label(
                                            ui,
                                            RichText::new(reason)
                                                .size(theme::SMALL_SIZE)
                                                .color(palette.muted),
                                            reason,
                                        );
                                    }
                                });
                            });
                        },
                    );
                });
            });
            ui.add_space(theme::SPACE_1);
        }

        if result.errors.is_empty() {
            components::muted(
                ui,
                "Classification comes from the URL and, when available, the server's Content-Type.",
            );
        }
    });
}

/// Scan notes rendered with the semantic colour of their severity.
fn scan_errors(ui: &mut Ui, errors: &[String], color: egui::Color32) {
    for error in errors.iter().take(6) {
        ui.label(RichText::new(error).size(theme::SMALL_SIZE).color(color));
    }
    if errors.len() > 6 {
        components::muted(ui, format!("…and {} more note(s).", errors.len() - 6));
    }
}

/// Destination picker plus the enqueue action for the ticked candidates.
fn queue_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    let destination = app
        .discovery_destination
        .clone()
        .or_else(|| app.add.effective_destination(&app.settings));

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_heading(ui, "Queue selected");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if components::button(ui, "Browse…", Action::Secondary).clicked()
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
        });

        ui.add_space(theme::SPACE_1);
        match &destination {
            Some(path) => {
                ui.label(
                    RichText::new(path.display().to_string())
                        .color(palette.text)
                        .monospace(),
                );
                if app.discovery_destination.is_none() {
                    components::muted(ui, "Using the folder from Settings.");
                }
            }
            None => {
                ui.label(RichText::new("No destination").color(palette.warning));
                components::muted(ui, NO_DESTINATION);
            }
        }

        ui.add_space(theme::SPACE_2);
        let selected_count = app.discovery.selected_count();
        let candidates = app.discovery.candidates_for_planner();
        let enabled = selected_count > 0 && destination.is_some();

        ui.horizontal(|ui| {
            components::status_badge(ui, &format!("{selected_count} selected"), palette.accent);
            ui.add_enabled_ui(enabled, |ui| {
                if components::button(ui, "Add Selected to Queue", Action::Start).clicked()
                    && let Some(destination) = destination.clone()
                {
                    app.send(Command::EnqueueCandidates {
                        candidates,
                        destination,
                    });
                }
            });
        });

        if selected_count == 0 {
            components::muted(
                ui,
                "Tick at least one selectable candidate to enable the queue action.",
            );
        } else if destination.is_none() {
            components::muted(ui, NO_DESTINATION);
        }
    });
}

/// Result of the last enqueue action.
fn outcome_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    let Some(outcome) = &app.discovery.outcome else {
        return;
    };

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Last queue action");
        ui.add_space(theme::SPACE_1);

        let accent = if outcome.failed.is_empty() {
            palette.success
        } else {
            palette.warning
        };
        components::status_badge(
            ui,
            &format!("{} queued · {} started", outcome.added, outcome.started),
            accent,
        );

        if !outcome.failed.is_empty() {
            ui.add_space(theme::SPACE_2);
            for (url, reason) in outcome.failed.iter().take(8) {
                ui.label(
                    RichText::new(format!("✗ {} — {reason}", redact_url(url)))
                        .size(theme::SMALL_SIZE)
                        .color(palette.error),
                );
            }
            if outcome.failed.len() > 8 {
                components::muted(
                    ui,
                    format!("…and {} more failure(s).", outcome.failed.len() - 8),
                );
            }
        }
    });
}

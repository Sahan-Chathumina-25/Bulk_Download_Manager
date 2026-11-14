//! AI File Finder: identify archive candidates, group multipart archives and
//! explain every recommendation.
//!
//! Like the other pages, this page performs no I/O: it sends the
//! analyze/cancel/enqueue commands and renders the state the backend reports.
//! Scores are advisory — nothing is queued or downloaded without the user
//! ticking rows and pressing the queue action.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::AiRunSource;
use crate::app::theme::{self, Action};
use crate::app::{BulkDownloadApp, components, format};
use crate::file_intelligence::multipart::GroupStatus;
use crate::file_intelligence::report::{AnalysisReport, CandidateReport, GroupReport};
use crate::file_intelligence::scoring::Confidence;
use crate::services::validation::redact_url;

const NO_DESTINATION: &str =
    "No destination folder set. Set one in Settings → Storage & history, or pick one here.";

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_header(
        ui,
        "AI File Finder",
        "Find archive downloads in links — with explained confidence",
        None,
    );

    input_card(app, ui);
    ui.add_space(theme::SPACE_2);

    if let Some(report) = app.ai.report.clone() {
        summary_tiles(ui, &report);
        ui.add_space(theme::SPACE_2);
        groups_card(app, ui, &report);
        ui.add_space(theme::SPACE_2);
        candidates_card(app, ui, &report);
        ui.add_space(theme::SPACE_2);
    }

    queue_card(app, ui);

    if app.ai.outcome.is_some() {
        ui.add_space(theme::SPACE_2);
        outcome_card(app, ui);
    }
}

/// URL input, options and the primary action.
fn input_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    let discovered_available = !app.discovery.candidates().is_empty();
    let use_discovered = app.ai.analyze_discovered;
    let can_run = !app.ai.analyzing
        && (use_discovered && discovered_available || !app.ai.urls_text.trim().is_empty());

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Find archive files");
        ui.add_space(theme::SPACE_1);

        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut app.ai.multiline, "Multiline URL input");
            ui.add_enabled_ui(discovered_available, |ui| {
                ui.checkbox(
                    &mut app.ai.analyze_discovered,
                    "Analyze the links from my last Discover scan",
                )
                .on_disabled_hover_text("Scan a page under Discover first");
            });
            ui.checkbox(&mut app.ai.probe_metadata, "Verify with a small HTTP check")
                .on_hover_text(
                    "Fetches headers (and at most the first kilobyte) for candidate links. \
                 Nothing is downloaded in full.",
                );
        });
        ui.add_space(theme::SPACE_1);

        if app.ai.multiline {
            ui.add(
                egui::TextEdit::multiline(&mut app.ai.urls_text)
                    .hint_text("One URL per line…")
                    .desired_rows(5)
                    .desired_width(ui.available_width()),
            );
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut app.ai.urls_text)
                    .hint_text("https://example.com/downloads")
                    .desired_width(ui.available_width()),
            );
        }
        ui.add_space(theme::SPACE_1);

        ui.horizontal(|ui| {
            let analyze_discovered = app.ai.analyze_discovered;
            ui.add_enabled_ui(can_run, |ui| {
                if components::button(ui, "Find Archive Files", Action::Discover).clicked() {
                    app.ai.analyzing = true;
                    app.ai.status = "Analyzing…".to_string();
                    app.ai.last_run = Some(if analyze_discovered {
                        AiRunSource::Discovered
                    } else {
                        AiRunSource::Pasted
                    });
                    if analyze_discovered {
                        app.send(Command::AnalyzeCandidates {
                            candidates: app.discovery.candidates().to_vec(),
                            probe: app.ai.probe_metadata,
                        });
                    } else {
                        app.send(Command::AnalyzeUrls {
                            text: app.ai.urls_text.clone(),
                            probe: app.ai.probe_metadata,
                        });
                    }
                }
            });

            if app.ai.analyzing {
                if components::button(ui, "Cancel analysis", Action::Cancel).clicked() {
                    app.send(Command::CancelAnalysis);
                }
            } else if app.ai.last_run.is_some()
                && components::button(ui, "Rescan", Action::Secondary).clicked()
            {
                match app.ai.last_run {
                    Some(AiRunSource::Discovered) if discovered_available => {
                        app.ai.analyzing = true;
                        app.ai.status = "Analyzing…".to_string();
                        app.send(Command::AnalyzeCandidates {
                            candidates: app.discovery.candidates().to_vec(),
                            probe: app.ai.probe_metadata,
                        });
                    }
                    Some(AiRunSource::Pasted) if !app.ai.urls_text.trim().is_empty() => {
                        app.ai.analyzing = true;
                        app.ai.status = "Analyzing…".to_string();
                        app.send(Command::AnalyzeUrls {
                            text: app.ai.urls_text.clone(),
                            probe: app.ai.probe_metadata,
                        });
                    }
                    _ => {}
                }
            } else if app.ai.report.is_some()
                && components::link_button(ui, "Clear results").clicked()
            {
                app.ai.clear();
            }
        });

        if app.ai.analyzing {
            ui.add_space(theme::SPACE_1);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(&app.ai.status)
                        .size(theme::SMALL_SIZE)
                        .color(palette.warning),
                );
            });
        } else if !app.ai.status.is_empty() {
            ui.add_space(theme::SPACE_1);
            components::muted(ui, app.ai.status.as_str());
        }

        ui.add_space(theme::SPACE_1);
        components::muted(
            ui,
            "Only links you are authorized to access are analyzed. \
             Ad and tracker destinations are filtered before any link is considered.",
        );
    });
}

/// Counts from the last run, rendered as glass tiles.
fn summary_tiles(ui: &mut Ui, report: &AnalysisReport) {
    let palette = theme::palette(ui);
    ui.horizontal_wrapped(|ui| {
        components::stat_tile(
            ui,
            "Files analyzed",
            report.analyzed_count.to_string(),
            palette.text,
            None::<fn()>,
        );
        components::stat_tile(
            ui,
            "Archive candidates",
            report.candidate_count.to_string(),
            palette.accent,
            None::<fn()>,
        );
        components::stat_tile(
            ui,
            "Multipart groups",
            report.groups.len().to_string(),
            palette.cyan,
            None::<fn()>,
        );
        components::stat_tile(
            ui,
            "Complete groups",
            report.complete_groups.to_string(),
            palette.success,
            None::<fn()>,
        );
        components::stat_tile(
            ui,
            "Incomplete groups",
            report.incomplete_groups.to_string(),
            palette.warning,
            None::<fn()>,
        );
        components::stat_tile(
            ui,
            "Filtered ad/tracker",
            report.filtered_count.to_string(),
            palette.muted,
            None::<fn()>,
        );
        components::stat_tile(
            ui,
            "Unverified",
            report.unverified_count.to_string(),
            palette.info,
            None::<fn()>,
        );
    });
}

/// Multipart group cards with expandable part lists.
fn groups_card(app: &mut BulkDownloadApp, ui: &mut Ui, report: &AnalysisReport) {
    if report.groups.is_empty() {
        return;
    }
    let palette = theme::palette(ui);
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            components::section_heading(ui, "Multipart groups");
            ui.add_space(theme::SPACE_2);
            components::status_badge(
                ui,
                &format!("{} group(s)", report.groups.len()),
                palette.cyan,
            );
            if components::link_button(ui, "Select complete groups").clicked() {
                app.ai.select_complete_groups(true);
            }
        });
        ui.add_space(theme::SPACE_2);

        for group in &report.groups {
            group_card(app, ui, group);
            ui.add_space(theme::SPACE_2);
        }
        components::muted(
            ui,
            "“Complete (per naming)” means the part numbering matches the format's \
             convention. Files are never verified as a whole until downloaded.",
        );
    });
}

fn group_card(app: &mut BulkDownloadApp, ui: &mut Ui, group: &GroupReport) {
    let palette = theme::palette(ui);
    let status_color = match group.status {
        GroupStatus::Complete => palette.success,
        GroupStatus::Incomplete => palette.warning,
        GroupStatus::Duplicate | GroupStatus::Inconsistent => palette.error,
        GroupStatus::Undetermined => palette.info,
    };
    let expanded = app.ai.groups_expanded.contains(&group.key);

    components::inset_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            let toggle = ui
                .label(RichText::new(if expanded { "▾" } else { "▸" }).color(palette.muted))
                .clicked();
            if toggle {
                if expanded {
                    app.ai.groups_expanded.remove(&group.key);
                } else {
                    app.ai.groups_expanded.insert(group.key.clone());
                }
            }

            ui.label(
                RichText::new(&group.display_name)
                    .strong()
                    .color(palette.text),
            );
            components::status_badge(ui, group.status.label(), status_color);
            components::muted(ui, group.archive_type.clone());
            components::muted(ui, format!("{} part(s)", group.parts.len()));
            components::muted(ui, format!("best score {}/100", group.score));
        });

        ui.horizontal_wrapped(|ui| {
            let labels: Vec<String> = group
                .parts
                .iter()
                .map(|part| {
                    if part.label.is_empty() {
                        format!("#{}", part.number)
                    } else {
                        part.label.clone()
                    }
                })
                .collect();
            components::muted(ui, format!("Parts: {}", labels.join(", ")));
        });
        if !group.missing.is_empty() {
            ui.label(
                RichText::new(format!("Missing: {}", group.missing.join(", ")))
                    .size(theme::SMALL_SIZE)
                    .color(palette.warning),
            );
        }
        if !group.duplicates.is_empty() {
            ui.label(
                RichText::new(format!("Duplicates: {}", group.duplicates.join(", ")))
                    .size(theme::SMALL_SIZE)
                    .color(palette.error),
            );
        }

        if expanded {
            ui.add_space(theme::SPACE_1);
            for (part_index, part) in group.parts.iter().enumerate() {
                let candidate_index = part.candidate_index;
                let selectable = app
                    .ai
                    .candidates()
                    .get(candidate_index)
                    .is_some_and(CandidateReport::is_selectable);
                let ticked = selectable && app.ai.is_selected(candidate_index);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(selectable && candidate_index != usize::MAX, |ui| {
                        let mut checked = ticked;
                        if ui.checkbox(&mut checked, "").changed() {
                            app.ai.toggle(candidate_index);
                        }
                    });
                    components::ellipsized_label(
                        ui,
                        RichText::new(&part.filename)
                            .size(theme::SMALL_SIZE)
                            .color(palette.text),
                        &part.filename,
                    );
                    if part_index + 1 == group.parts.len() {
                        // Show the last part's numbering explicitly.
                        components::muted(ui, format!("(#{})", part.number));
                    }
                });
            }
        }
    });
}

/// Ranked candidate rows with confidence badges and evidence panels.
fn candidates_card(app: &mut BulkDownloadApp, ui: &mut Ui, report: &AnalysisReport) {
    let palette = theme::palette(ui);

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            components::section_heading(ui, "Candidates");
            ui.add_space(theme::SPACE_2);
            components::status_badge(
                ui,
                &format!("{} analyzed", report.analyzed_count),
                palette.text_secondary,
            );
            components::status_badge(
                ui,
                &format!("{} selected", app.ai.selected_count()),
                palette.accent,
            );
            if components::link_button(ui, "Select all").clicked() {
                app.ai.set_all(true);
            }
            if components::link_button(ui, "Deselect all").clicked() {
                app.ai.set_all(false);
            }
        });
        ui.add_space(theme::SPACE_2);

        if report.candidates.is_empty() {
            components::empty_state_action(
                ui,
                "No links were analyzed",
                "Paste URLs above (or scan a page under Discover first), then run the finder.",
                "",
            );
            return;
        }

        let rows: Vec<CandidateReport> = report.candidates.clone();
        for (index, candidate) in rows.iter().enumerate() {
            candidate_row(app, ui, index, candidate);
            ui.add_space(theme::SPACE_1);
        }

        components::muted(
            ui,
            "Scores describe the weight of the evidence, not a probability. \
             Low-confidence and unknown links stay available for manual review.",
        );
    });
}

fn candidate_row(
    app: &mut BulkDownloadApp,
    ui: &mut Ui,
    index: usize,
    candidate: &CandidateReport,
) {
    let palette = theme::palette(ui);
    let selectable = candidate.is_selectable();
    let ticked = selectable && app.ai.is_selected(index);
    let expanded = app.ai.expanded.contains(&index);

    let score_color = match candidate.confidence {
        Confidence::VeryHigh => palette.success,
        Confidence::High => palette.success,
        Confidence::Possible => palette.warning,
        Confidence::Low => palette.text_secondary,
        Confidence::Unlikely => palette.muted,
    };
    let status_color = if candidate.filtered_reason.is_some()
        || candidate.status.starts_with("Webpage")
        || candidate.status.starts_with("Not an archive")
    {
        palette.muted
    } else if candidate.status.starts_with("Unverified") {
        palette.info
    } else {
        palette.success
    };

    components::download_row(ui, ticked, |ui| {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(selectable, |ui| {
                let mut checked = ticked;
                if ui.checkbox(&mut checked, "").changed() {
                    app.ai.toggle(index);
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} / 100", candidate.score))
                        .size(theme::SMALL_SIZE)
                        .strong()
                        .color(score_color),
                );
                if let Some(size) = candidate.size {
                    ui.label(
                        RichText::new(format::format_bytes(Some(size)))
                            .size(theme::SMALL_SIZE)
                            .color(palette.muted),
                    );
                }
                if let Some(group) = &candidate.group_key {
                    ui.label(
                        RichText::new(format!("group: {group}"))
                            .size(theme::SMALL_SIZE)
                            .color(palette.cyan),
                    )
                    .on_hover_text(group);
                }

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.vertical(|ui| {
                        let mut tooltip = redact_url(&candidate.url);
                        if !candidate.source_page_url.is_empty() {
                            tooltip.push_str(&format!(
                                "\nFrom: {}",
                                redact_url(&candidate.source_page_url)
                            ));
                        }
                        components::ellipsized_label(
                            ui,
                            RichText::new(&candidate.filename)
                                .size(theme::BODY_SIZE)
                                .color(if selectable {
                                    palette.text
                                } else {
                                    palette.muted
                                }),
                            &tooltip,
                        );
                        ui.horizontal_wrapped(|ui| {
                            components::muted(ui, candidate.archive_type.clone());
                            components::status_badge(ui, &candidate.status, status_color);
                            let evidence_label = if expanded {
                                "Hide evidence"
                            } else {
                                "View evidence"
                            };
                            if components::link_button(ui, evidence_label).clicked() {
                                if expanded {
                                    app.ai.expanded.remove(&index);
                                } else {
                                    app.ai.expanded.insert(index);
                                }
                            }
                        });
                    });
                });
            });
        });

        if expanded {
            evidence_panel(ui, candidate);
        }
    });
}

/// Plain-English explanation of one candidate's classification.
fn evidence_panel(ui: &mut Ui, candidate: &CandidateReport) {
    let palette = theme::palette(ui);
    components::inset_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Why this classification");
        ui.add_space(theme::SPACE_1);

        if candidate.filtered_reason.is_none() {
            ui.label(
                RichText::new(
                    "✓ The URL passed security validation (syntax, scheme and address checks).",
                )
                .size(theme::SMALL_SIZE)
                .color(palette.success),
            );
        }

        for evidence in &candidate.evidence {
            let sign = if evidence.weight > 0 {
                "+"
            } else if evidence.weight < 0 {
                "−"
            } else {
                "·"
            };
            let color = if evidence.weight > 0 {
                palette.success
            } else if evidence.weight < 0 {
                palette.error
            } else {
                palette.muted
            };
            ui.label(
                RichText::new(format!(
                    "{sign}{} · {}",
                    evidence.weight.abs(),
                    evidence.reason
                ))
                .size(theme::SMALL_SIZE)
                .color(color),
            );
        }

        if candidate.filtered_reason.is_some() {
            ui.label(
                RichText::new(format!(
                    "✗ Matched an ad/tracker rule: {}",
                    candidate.filtered_reason.as_deref().unwrap_or_default()
                ))
                .size(theme::SMALL_SIZE)
                .color(palette.error),
            );
        }

        if !candidate.missing_evidence.is_empty() {
            ui.add_space(theme::SPACE_1);
            ui.label(
                RichText::new("Not available for this link:")
                    .size(theme::SMALL_SIZE)
                    .strong()
                    .color(palette.text_secondary),
            );
            for note in &candidate.missing_evidence {
                components::muted(ui, note.clone());
            }
        }
    });
}

/// Destination picker plus the enqueue action.
fn queue_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    let destination = app
        .ai_destination
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
                    app.ai_destination = Some(folder);
                }
                if app.ai_destination.is_some()
                    && components::link_button(ui, "Use default").clicked()
                {
                    app.ai_destination = None;
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
                if app.ai_destination.is_none() {
                    components::muted(ui, "Using the folder from Settings.");
                }
            }
            None => {
                ui.label(RichText::new("No destination").color(palette.warning));
                components::muted(ui, NO_DESTINATION);
            }
        }

        ui.add_space(theme::SPACE_2);
        let selected_count = app.ai.selected_count();
        let candidates = app.ai.selected_for_planner();
        let incomplete_warning = app.ai.selected_has_incomplete_groups();
        let enabled = selected_count > 0 && destination.is_some();

        if incomplete_warning {
            ui.label(
                RichText::new(
                    "⚠ Some selected files belong to multipart groups that look incomplete. \
                     They may not extract without their missing parts.",
                )
                .size(theme::SMALL_SIZE)
                .color(palette.warning),
            );
        }

        ui.horizontal(|ui| {
            components::status_badge(ui, &format!("{selected_count} selected"), palette.accent);
            ui.add_enabled_ui(enabled, |ui| {
                if components::button(ui, "Add Selected to Queue", Action::Start).clicked()
                    && let Some(destination) = destination.clone()
                {
                    app.send(Command::EnqueueAiCandidates {
                        candidates,
                        destination,
                    });
                }
            });
        });

        if selected_count == 0 {
            components::muted(
                ui,
                "Tick at least one candidate to enable the queue action. \
                 Nothing is ever added automatically.",
            );
        }
    });
}

/// Result of the last enqueue action.
fn outcome_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    let Some(outcome) = &app.ai.outcome else {
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

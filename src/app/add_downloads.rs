//! Add Downloads: paste URLs, pick a destination, optionally override the name.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::Severity;
use crate::app::{BulkDownloadApp, components, theme};
use crate::services::validation::redact_url;

/// Legacy copy shown when neither the form nor the settings name a folder.
const NO_DESTINATION: &str =
    "No destination folder set. Set one here (or in Settings → Destination folder).";
/// Legacy copy for an empty submit.
const EMPTY_INPUT: &str = "Nothing to add — enter at least one http/https URL per line.";

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(ui, "Add Downloads", "Paste one http/https URL per line.");

    track_single_url(app);

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "URLs");
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut app.add.urls_text)
                .desired_rows(9)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com/file.zip"),
        );
        ui.add_space(4.0);
        components::muted(
            ui,
            "Blank lines are ignored and repeated URLs are collapsed.",
        );
    });

    ui.add_space(8.0);

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "Destination");
        ui.add_space(4.0);

        let effective = app.add.effective_destination(&app.settings);
        ui.horizontal(|ui| {
            match &effective {
                Some(path) => ui.label(
                    RichText::new(path.display().to_string())
                        .color(theme::TEXT)
                        .monospace(),
                ),
                None => ui.label(RichText::new("Not set").color(theme::WARN)),
            };
            if components::secondary_button(ui, "Browse…").clicked()
                && let Some(folder) = rfd::FileDialog::new().pick_folder()
            {
                app.add.destination = Some(folder);
            }
            if app.add.destination.is_some() && components::link_button(ui, "Use default").clicked()
            {
                app.add.destination = None;
            }
        });

        if effective.is_none() {
            components::muted(ui, NO_DESTINATION);
        } else if app.add.destination.is_none() {
            components::muted(ui, "Using the folder from Settings.");
        }
    });

    ui.add_space(8.0);

    let single = app.add_line_count == 1;
    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "Overrides");
        components::muted(
            ui,
            "A filename and checksum apply only when exactly one URL is entered.",
        );
        ui.add_space(4.0);

        ui.add_enabled_ui(single, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Filename").color(theme::MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut app.add.filename_override)
                        .hint_text("keep the server's name")
                        .desired_width(320.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("SHA-256").color(theme::MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut app.add.checksum)
                        .hint_text("optional 64-character hex digest")
                        .desired_width(320.0),
                );
            });
        });
    });

    ui.add_space(10.0);

    ui.horizontal(|ui| {
        let text = app.add.urls_text.clone();
        let destination = app.add.effective_destination(&app.settings);

        if components::primary_button(ui, "Add downloads").clicked() {
            if text.trim().is_empty() {
                app.toast(Severity::Warning, EMPTY_INPUT);
            } else if destination.is_none() {
                app.toast(Severity::Warning, NO_DESTINATION);
            } else {
                let filename = single_override(&app.add.filename_override);
                let checksum = single_override(&app.add.checksum);
                app.send(Command::AddUrls {
                    text: text.clone(),
                    destination: app.add.destination.clone(),
                    filename,
                    checksum,
                    start_now: app.add.start_immediately,
                });
            }
        }

        if components::secondary_button(ui, "Check URLs").clicked() {
            if text.trim().is_empty() {
                app.toast(Severity::Warning, EMPTY_INPUT);
            } else {
                app.send(Command::PreviewUrls { text });
            }
        }

        if components::link_button(ui, "Clear").clicked() {
            app.add.urls_text.clear();
            app.add.filename_override.clear();
            app.add.checksum.clear();
            app.add.clear_feedback();
            app.add_line_count = 0;
        }

        ui.checkbox(&mut app.add.start_immediately, "Start immediately");
    });

    if let Some(error) = &app.add.error {
        ui.add_space(6.0);
        ui.label(RichText::new(error).color(theme::ERROR));
    }

    if let Some(preview) = &app.add.preview {
        ui.add_space(8.0);
        components::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_title(ui, "Check result");
            components::muted(
                ui,
                format!(
                    "{} accepted · {} duplicate(s) · {} rejected",
                    preview.accepted.len(),
                    preview.duplicates.len(),
                    preview.rejected.len()
                ),
            );
            for (url, reason) in preview.rejected.iter().take(8) {
                ui.label(
                    RichText::new(format!("✗ {} — {reason}", redact_url(url)))
                        .color(theme::ERROR)
                        .size(12.0),
                );
            }
            if preview.rejected.len() > 8 {
                components::muted(ui, format!("…and {} more.", preview.rejected.len() - 8));
            }
        });
    }

    if let Some(outcome) = &app.add.outcome {
        ui.add_space(8.0);
        components::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_title(ui, "Result");
            ui.label(
                RichText::new(format!(
                    "Added {} download(s); {} started.",
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
            if !outcome.failed.is_empty() {
                components::muted(ui, format!("{} failure(s) total.", outcome.failed.len()));
            }
        });
    }
}

/// Keep the single-URL rule in sync with the text box.
fn track_single_url(app: &mut BulkDownloadApp) {
    let count = app
        .add
        .urls_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if count != app.add_line_count {
        app.add_line_count = count;
        // A filename or checksum must never leak onto a different batch.
        if count != 1 {
            app.add.filename_override.clear();
            app.add.checksum.clear();
        }
    }
}

/// Trim an optional override; an empty box means "no override".
fn single_override(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

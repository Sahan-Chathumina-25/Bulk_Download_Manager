//! Add Downloads: paste or import URLs, then queue them.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::Severity;
use crate::app::theme::Action;
use crate::app::{BulkDownloadApp, components, theme};
use crate::services::validation::redact_url;

/// Shown when neither the form nor the settings name a folder.
const NO_DESTINATION: &str =
    "No destination folder set. Set one here (or in Settings → Destination folder).";
/// Shown for a submit with no URL lines.
const EMPTY_INPUT: &str = "Nothing to add — enter at least one http/https URL per line.";
/// Helper under the editor; the tab rule matches the parser.
const EDITOR_HELP: &str = "One URL per line. A second tab-separated column is ignored, blank lines are skipped and exact duplicates are collapsed.";

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    components::page_heading(
        ui,
        "Add Downloads",
        "Paste URLs or import a text file, choose a destination, then queue them.",
    );

    track_single_url(app);

    let has_lines = app
        .add
        .urls_text
        .lines()
        .any(|line| !line.trim().is_empty());
    let valid_entries = app
        .add
        .preview
        .as_ref()
        .map_or(0, |preview| preview.accepted.len());
    let can_add = if app.add.preview.is_some() {
        valid_entries > 0
    } else {
        has_lines
    };

    ui.horizontal(|ui| {
        if components::button(ui, "Import TXT…", Action::Import).clicked() {
            import_text_file(app);
        }
        if components::button(ui, "Preview", Action::Info).clicked() {
            if has_lines {
                app.add.error = None;
                app.add.outcome = None;
                app.add.preview_pending = true;
                app.send(Command::PreviewUrls {
                    text: app.add.urls_text.clone(),
                });
            } else {
                app.toast(Severity::Warning, EMPTY_INPUT);
            }
        }
        if components::link_button(ui, "Clear").clicked() {
            app.add.urls_text.clear();
            app.add.filename_override.clear();
            app.add.checksum.clear();
            app.add.clear_feedback();
            app.add_line_count = 0;
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_enabled_ui(can_add && !app.add.add_pending, |ui| {
                if components::button(ui, "Add to Queue", Action::Add).clicked() {
                    submit(app);
                }
            });
            ui.add_space(theme::SPACE_2);
            ui.checkbox(&mut app.add.start_immediately, "Start immediately");
        });
    });

    ui.add_space(theme::SPACE_2);

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            components::section_heading(ui, "URLs");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if app.add.preview_pending {
                    ui.label(
                        RichText::new("Checking URLs…")
                            .size(theme::SMALL_SIZE)
                            .color(palette.warning),
                    );
                } else if app.add.add_pending {
                    ui.label(
                        RichText::new("Adding to the queue…")
                            .size(theme::SMALL_SIZE)
                            .color(palette.accent),
                    );
                }
            });
        });
        ui.add_space(theme::SPACE_2);
        ui.add(
            egui::TextEdit::multiline(&mut app.add.urls_text)
                .desired_rows(8)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com/file.zip\nhttps://example.com/another-file.zip"),
        );
        ui.add_space(theme::SPACE_1);
        components::muted(ui, EDITOR_HELP);
    });

    ui.add_space(theme::SPACE_2);

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Destination");
        ui.add_space(theme::SPACE_1);

        let effective = app.add.effective_destination(&app.settings);
        ui.horizontal(|ui| {
            match &effective {
                Some(path) => ui.label(
                    RichText::new(path.display().to_string())
                        .color(palette.text)
                        .monospace(),
                ),
                None => ui.label(RichText::new("Not set").color(palette.warning)),
            };
            if components::button(ui, "Browse…", Action::Secondary).clicked()
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

    ui.add_space(theme::SPACE_2);

    let single = app.add_line_count == 1;
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Overrides");
        components::muted(
            ui,
            "A filename and checksum apply only when exactly one URL is entered.",
        );
        ui.add_space(theme::SPACE_1);

        ui.add_enabled_ui(single, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Filename").color(palette.muted));
                ui.add(
                    egui::TextEdit::singleline(&mut app.add.filename_override)
                        .hint_text("keep the server's name")
                        .desired_width(320.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("SHA-256").color(palette.muted));
                ui.add(
                    egui::TextEdit::singleline(&mut app.add.checksum)
                        .hint_text("optional 64-character hex digest")
                        .desired_width(320.0),
                );
            });
        });
    });

    if let Some(error) = &app.add.error {
        ui.add_space(theme::SPACE_2);
        components::status_badge(ui, error, palette.error);
    }

    if let Some(preview) = &app.add.preview {
        ui.add_space(theme::SPACE_2);
        components::card_for(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_heading(ui, "Preview");
            ui.add_space(theme::SPACE_1);

            ui.horizontal_wrapped(|ui| {
                components::status_badge(
                    ui,
                    &format!("{} valid", preview.accepted.len()),
                    palette.success,
                );
                components::status_badge(
                    ui,
                    &format!("{} duplicate(s)", preview.duplicates.len()),
                    palette.warning,
                );
                components::status_badge(
                    ui,
                    &format!("{} invalid", preview.rejected.len()),
                    palette.error,
                );
            });

            if preview.accepted.is_empty()
                && preview.duplicates.is_empty()
                && preview.rejected.is_empty()
            {
                components::muted(ui, "No URLs found in the editor.");
            }

            if !preview.duplicates.is_empty() {
                ui.add_space(theme::SPACE_2);
                for url in preview.duplicates.iter().take(5) {
                    ui.label(
                        RichText::new(format!("Duplicate — {}", redact_url(url)))
                            .size(theme::SMALL_SIZE)
                            .color(palette.warning),
                    );
                }
                if preview.duplicates.len() > 5 {
                    components::muted(
                        ui,
                        format!("…and {} more duplicate(s).", preview.duplicates.len() - 5),
                    );
                }
            }

            if !preview.rejected.is_empty() {
                ui.add_space(theme::SPACE_2);
                for (url, reason) in preview.rejected.iter().take(8) {
                    ui.label(
                        RichText::new(format!("Invalid — {} — {reason}", redact_url(url)))
                            .size(theme::SMALL_SIZE)
                            .color(palette.error),
                    );
                }
                if preview.rejected.len() > 8 {
                    components::muted(ui, format!("…and {} more.", preview.rejected.len() - 8));
                }
            }
        });
    }

    if let Some(outcome) = &app.add.outcome {
        ui.add_space(theme::SPACE_2);
        components::card_for(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            components::section_heading(ui, "Result");
            ui.add_space(theme::SPACE_1);
            let accent = if outcome.failed.is_empty() {
                palette.success
            } else {
                palette.warning
            };
            components::status_badge(
                ui,
                &format!("{} added · {} started", outcome.added, outcome.started),
                accent,
            );

            if !outcome.failed.is_empty() {
                ui.add_space(theme::SPACE_2);
                for (url, reason) in outcome.failed.iter().take(8) {
                    ui.label(
                        RichText::new(format!("Failed — {} — {reason}", redact_url(url)))
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
}

/// Validate the form and hand the batch to the backend.
fn submit(app: &mut BulkDownloadApp) {
    let destination = app.add.effective_destination(&app.settings);
    if destination.is_none() {
        app.toast(Severity::Warning, NO_DESTINATION);
        return;
    }

    let filename = single_override(&app.add.filename_override);
    let checksum = single_override(&app.add.checksum);
    app.add.error = None;
    app.add.outcome = None;
    app.add.add_pending = true;
    app.send(Command::AddUrls {
        text: app.add.urls_text.clone(),
        destination: app.add.destination.clone(),
        filename,
        checksum,
        start_now: app.add.start_immediately,
    });
}

/// Append the contents of a picked `.txt` file to the URL editor.
fn import_text_file(app: &mut BulkDownloadApp) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Text files", &["txt"])
        .pick_file()
    else {
        return;
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let trimmed = contents.trim_end();
            if trimmed.is_empty() {
                app.toast(Severity::Info, "The selected file is empty.");
                return;
            }
            if !app.add.urls_text.is_empty() && !app.add.urls_text.ends_with('\n') {
                app.add.urls_text.push('\n');
            }
            app.add.urls_text.push_str(trimmed);
            app.add.urls_text.push('\n');
            app.add.clear_feedback();
        }
        Err(error) => app.toast(
            Severity::Error,
            format!("Could not read {}: {error}", path.display()),
        ),
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

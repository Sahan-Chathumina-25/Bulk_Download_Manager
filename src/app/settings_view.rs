//! Settings: every persisted field plus Save / Reload.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::Severity;
use crate::app::{BulkDownloadApp, components, theme};
use crate::config::{CollisionPolicy, Theme};

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(
        ui,
        "Settings",
        "Changes are stored in the database and applied immediately where possible.",
    );

    downloads_card(app, ui);
    ui.add_space(8.0);
    discovery_card(app, ui);
    ui.add_space(8.0);
    appearance_card(app, ui);
    ui.add_space(10.0);
    footer(app, ui);
}

fn downloads_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "Downloads");
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Destination folder").color(theme::MUTED));
            ui.add(
                egui::TextEdit::singleline(&mut app.settings_state.draft.default_destination_dir)
                    .desired_width(360.0)
                    .hint_text("C:\\Downloads"),
            );
            if components::secondary_button(ui, "Browse…").clicked()
                && let Some(folder) = rfd::FileDialog::new().pick_folder()
            {
                app.settings_state.draft.default_destination_dir = folder.display().to_string();
            }
            if components::link_button(ui, "Clear").clicked() {
                app.settings_state.draft.default_destination_dir.clear();
            }
        });

        ui.add_space(6.0);

        egui::Grid::new("settings_numbers")
            .num_columns(2)
            .spacing([18.0, 8.0])
            .show(ui, |ui| {
                number_row(
                    ui,
                    "Max simultaneous",
                    &mut app.settings_state.max_simultaneous_text,
                    "How many downloads may run at once. Applies immediately.",
                );
                number_row(
                    ui,
                    "Connection timeout (s)",
                    &mut app.settings_state.connection_timeout_text,
                    "Seconds to wait for a connection before failing.",
                );
                number_row(
                    ui,
                    "Retry count",
                    &mut app.settings_state.retry_count_text,
                    "Transient failures are retried this many times.",
                );
                number_row(
                    ui,
                    "Retry delay (s)",
                    &mut app.settings_state.retry_delay_text,
                    "Base delay between retries; it grows exponentially.",
                );
                number_row(
                    ui,
                    "Buffer size (bytes)",
                    &mut app.settings_state.buffer_size_text,
                    "Read buffer used while streaming to disk.",
                );
                number_row(
                    ui,
                    "Bandwidth limit (KB/s)",
                    &mut app.settings_state.bandwidth_kbps_text,
                    "0 means unlimited.",
                );
            });

        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Existing files").color(theme::MUTED));
            let mut policy = app.settings_state.draft.filename_collision;
            egui::ComboBox::from_id_salt("collision_policy")
                .selected_text(collision_label(policy))
                .show_ui(ui, |ui| {
                    for candidate in [
                        CollisionPolicy::KeepExisting,
                        CollisionPolicy::Rename,
                        CollisionPolicy::Overwrite,
                        CollisionPolicy::Skip,
                    ] {
                        ui.selectable_value(&mut policy, candidate, collision_label(candidate));
                    }
                });
            app.settings_state.draft.filename_collision = policy;
        });

        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("User agent").color(theme::MUTED));
            ui.add(
                egui::TextEdit::singleline(&mut app.settings_state.user_agent_text)
                    .desired_width(460.0),
            );
        });
    });
}

fn discovery_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "Discovery");
        ui.add_space(6.0);

        ui.checkbox(
            &mut app.settings_state.draft.include_unknown_types,
            "Include unknown types when scanning pages",
        );
        ui.checkbox(
            &mut app.settings_state.draft.allow_private_addresses,
            "Allow private / loopback addresses (LAN testing only)",
        );
        ui.checkbox(
            &mut app.settings_state.draft.respect_rate_limit,
            "Respect server rate limits",
        );

        ui.add_space(6.0);
        ui.label(RichText::new("Allowed extensions").color(theme::MUTED));
        ui.add(
            egui::TextEdit::multiline(&mut app.settings_state.allowed_extensions_text)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("zip, pdf, mp4"),
        );
        components::muted(ui, "Comma or space separated; a leading dot is ignored.");
    });
}

fn appearance_card(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "Appearance & notifications");
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Theme").color(theme::MUTED));
            let mut choice = app.settings_state.draft.theme;
            egui::ComboBox::from_id_salt("theme_choice")
                .selected_text(theme_label(choice))
                .show_ui(ui, |ui| {
                    for candidate in [Theme::Dark, Theme::Light, Theme::System] {
                        ui.selectable_value(&mut choice, candidate, theme_label(candidate));
                    }
                });
            if choice != app.settings_state.draft.theme {
                app.settings_state.draft.theme = choice;
                // Preview immediately; the choice is persisted on Save.
                super::apply_theme(ui.ctx(), choice);
            }
        });

        ui.checkbox(
            &mut app.settings_state.draft.notifications,
            "Show desktop-style notifications inside the app",
        );
    });
}

fn footer(app: &mut BulkDownloadApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        if components::primary_button(ui, "Save settings").clicked() {
            match app.settings_state.validated_settings() {
                Ok(settings) => app.send(Command::SaveSettings { settings }),
                Err(error) => {
                    app.settings_state.set_error(Some(error.clone()));
                    app.toast(Severity::Error, error);
                }
            }
        }
        if components::secondary_button(ui, "Reload").clicked() {
            let settings = app.settings.clone();
            app.settings_state.load(settings);
        }

        if app.settings_state.is_dirty() {
            ui.label(
                RichText::new("Unsaved changes")
                    .color(theme::WARN)
                    .size(12.0),
            );
        }
    });

    if let Some(error) = &app.settings_state.error {
        ui.add_space(6.0);
        ui.label(RichText::new(error).color(theme::ERROR));
    }
}

fn number_row(ui: &mut Ui, label: &str, buffer: &mut String, hint: &str) {
    ui.label(RichText::new(label).color(theme::MUTED))
        .on_hover_text(hint);
    ui.add(egui::TextEdit::singleline(buffer).desired_width(140.0));
    ui.end_row();
}

fn collision_label(policy: CollisionPolicy) -> &'static str {
    match policy {
        CollisionPolicy::KeepExisting => "Keep existing",
        CollisionPolicy::Rename => "Rename",
        CollisionPolicy::Overwrite => "Overwrite",
        CollisionPolicy::Skip => "Skip",
    }
}

fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "Dark",
        Theme::Light => "Light",
        Theme::System => "System",
    }
}

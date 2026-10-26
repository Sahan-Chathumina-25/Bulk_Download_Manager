//! Settings: every persisted field grouped by purpose, plus Save / Reload.
//!
//! The page edits a draft only. Theme is previewed immediately because it is a
//! view concern, but every other value is persisted when Save is pressed, so
//! the page never claims a change was applied when it was not.

use egui::{RichText, Ui};

use crate::app::backend::Command;
use crate::app::state::Severity;
use crate::app::theme::{self, Palette};
use crate::app::{BulkDownloadApp, components};
use crate::config::{CollisionPolicy, Theme};

/// Width reserved for the label + hint column in the two-column grids. Without
/// this the value widget wins the width and labels wrap per character.
const LABEL_COLUMN_WIDTH: f32 = 240.0;

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    // The shell already scrolls the page column; a nested vertical scroll area
    // here collapses the body, so the page only contributes its content.
    ui.set_width(ui.available_width());
    components::page_heading(
        ui,
        "Settings",
        "Preferences are stored in the local database. Theme previews immediately; every other change is saved with Save settings.",
    );

    startup_banner(app, ui, palette);
    downloads_card(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    appearance_card(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    network_card(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    discovery_card(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    storage_card(app, ui, palette);
    ui.add_space(theme::SPACE_3);
    footer(app, ui, palette);
}

/// Warning for a settings row that could not be read at startup.
fn startup_banner(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    let Some(error) = app.settings_error.clone() else {
        return;
    };

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Saved settings could not be loaded")
                    .size(theme::BODY_SIZE)
                    .strong()
                    .color(palette.error),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if components::link_button(ui, "Dismiss").clicked() {
                    app.settings_error = None;
                }
            });
        });
        ui.label(
            RichText::new(error)
                .size(theme::SMALL_SIZE)
                .color(palette.error),
        );
        components::muted(
            ui,
            "Defaults are in use for this session. Saving settings replaces the unreadable row.",
        );
    });
    ui.add_space(theme::SPACE_2);
}

fn downloads_card(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Downloads");
        ui.add_space(theme::SPACE_1);

        egui::Grid::new("settings_downloads")
            .num_columns(2)
            .min_col_width(LABEL_COLUMN_WIDTH)
            .spacing([24.0, theme::SPACE_3])
            .show(ui, |ui| {
                number_row(
                    ui,
                    "Max simultaneous",
                    "How many downloads may transfer at once. Clamped to 1–10 on save.",
                    &mut app.settings_state.max_simultaneous_text,
                    palette,
                );
                number_row(
                    ui,
                    "Retry count",
                    "Transient failures are retried this many times (0–20).",
                    &mut app.settings_state.retry_count_text,
                    palette,
                );
                number_row(
                    ui,
                    "Retry delay (s)",
                    "Base delay between retries; the wait grows exponentially.",
                    &mut app.settings_state.retry_delay_text,
                    palette,
                );
            });

        components::muted(ui, "These values apply to downloads started after saving.");
    });
}

fn appearance_card(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Appearance");
        ui.add_space(theme::SPACE_1);

        ui.label(
            RichText::new("Theme")
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
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
        components::muted(
            ui,
            "Dark and Light use the Midnight Indigo palettes. System follows the OS preference and falls back to Dark when the platform does not report one.",
        );
        components::muted(ui, "The preview is immediate; Save persists the choice.");

        ui.add_space(theme::SPACE_2);
        checkbox_row(
            ui,
            &mut app.settings_state.draft.notifications,
            "Show notifications inside the app",
            "Surfaces download and error events as transient messages in the app window.",
            palette,
        );
    });
}

fn network_card(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Network & performance");
        ui.add_space(theme::SPACE_1);

        egui::Grid::new("settings_network")
            .num_columns(2)
            .min_col_width(LABEL_COLUMN_WIDTH)
            .spacing([24.0, theme::SPACE_3])
            .show(ui, |ui| {
                number_row(
                    ui,
                    "Connection timeout (s)",
                    "Seconds to wait for a connection before the attempt fails.",
                    &mut app.settings_state.connection_timeout_text,
                    palette,
                );
                number_row(
                    ui,
                    "Buffer size (bytes)",
                    "Read buffer used while streaming to disk. Must be greater than 0.",
                    &mut app.settings_state.buffer_size_text,
                    palette,
                );
                number_row(
                    ui,
                    "Bandwidth limit (KB/s)",
                    "0 means unlimited. Saved as bytes per second.",
                    &mut app.settings_state.bandwidth_kbps_text,
                    palette,
                );
            });

        ui.add_space(theme::SPACE_2);
        ui.label(
            RichText::new("User agent")
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
        ui.add(
            egui::TextEdit::singleline(&mut app.settings_state.user_agent_text)
                .desired_width(f32::INFINITY)
                .hint_text("BulkDownloadManager/0.1"),
        );
        components::muted(
            ui,
            "Sent with discovery and download requests. It cannot be empty.",
        );
    });
}

fn discovery_card(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Discovery & filtering");
        ui.add_space(theme::SPACE_1);

        checkbox_row(
            ui,
            &mut app.settings_state.draft.include_unknown_types,
            "Include unknown types when scanning pages",
            "Offer links without a known extension too. Verify them before queueing; they may be landing pages.",
            palette,
        );
        checkbox_row(
            ui,
            &mut app.settings_state.draft.ad_filter_enabled,
            "Filter ad and tracker hosts while scanning",
            "Matching links stay visible with a reason but cannot be selected or queued.",
            palette,
        );
        checkbox_row(
            ui,
            &mut app.settings_state.draft.respect_rate_limit,
            "Respect server rate limits",
            "Slow down and back off when a server asks the client to.",
            palette,
        );
        checkbox_row(
            ui,
            &mut app.settings_state.draft.allow_private_addresses,
            "Allow private and loopback addresses",
            "Security escape hatch for deliberate LAN testing only. Leave this off for normal browsing.",
            palette,
        );

        ui.add_space(theme::SPACE_2);
        ui.label(
            RichText::new("Allowed extensions")
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
        ui.add(
            egui::TextEdit::multiline(&mut app.settings_state.allowed_extensions_text)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("zip, pdf, mp4"),
        );
        components::muted(
            ui,
            "Comma or space separated. A leading dot is ignored; each name is at most 10 alphanumeric characters.",
        );

        ui.add_space(theme::SPACE_2);
        ui.label(
            RichText::new("Custom filter list (hosts format)")
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
        ui.add(
            egui::TextEdit::singleline(&mut app.settings_state.draft.ad_filter_list_path)
                .desired_width(f32::INFINITY)
                .hint_text("e.g. adblock-hosts.txt"),
        );
        components::muted(
            ui,
            "Optional extra denylist. The built-in list always applies; an unreadable custom list is reported and ignored.",
        );
    });
}

fn storage_card(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_heading(ui, "Storage & history");
        ui.add_space(theme::SPACE_1);

        ui.label(
            RichText::new("Default destination folder")
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(
                    &mut app.settings_state.draft.default_destination_dir,
                )
                .desired_width(420.0)
                .hint_text("C:\\Downloads"),
            );
            if components::secondary_button(ui, "Browse…").clicked()
                && let Some(folder) = rfd::FileDialog::new().pick_folder()
            {
                app.settings_state.draft.default_destination_dir =
                    folder.display().to_string();
            }
            if components::link_button(ui, "Clear").clicked() {
                app.settings_state.draft.default_destination_dir.clear();
            }
        });
        components::muted(
            ui,
            "Used by new downloads. Add Downloads and Discover can override it for one job.",
        );

        ui.add_space(theme::SPACE_2);
        ui.label(
            RichText::new("When the destination file already exists")
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
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
                    ui.selectable_value(
                        &mut policy,
                        candidate,
                        collision_label(candidate),
                    );
                }
            });
        app.settings_state.draft.filename_collision = policy;
        components::muted(ui, collision_hint(policy));

        ui.add_space(theme::SPACE_2);
        components::muted(
            ui,
            "History lives in the local database. Clear it from the History page; that dialog decides whether downloaded files are also deleted.",
        );
    });
}

fn footer(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
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
            app.settings_error = None;
        }

        if app.settings_state.is_dirty() {
            components::status_badge(ui, "Unsaved changes", palette.warning);
        }
    });

    if let Some(error) = &app.settings_state.error {
        ui.add_space(theme::SPACE_1);
        ui.label(
            RichText::new(error)
                .size(theme::SMALL_SIZE)
                .color(palette.error),
        );
    }
}

/// Numeric field with a visible label and a one-line explanation.
fn number_row(ui: &mut Ui, label: &str, hint: &str, buffer: &mut String, palette: Palette) {
    field_label(ui, label, hint, palette);
    ui.add(egui::TextEdit::singleline(buffer).desired_width(160.0));
    ui.end_row();
}

/// Checkbox with a supporting explanation underneath.
fn checkbox_row(ui: &mut Ui, checked: &mut bool, label: &str, hint: &str, palette: Palette) {
    ui.vertical(|ui| {
        ui.checkbox(checked, label);
        ui.label(
            RichText::new(hint)
                .size(theme::SMALL_SIZE)
                .color(palette.muted),
        );
    });
}

/// Label plus explanation, reused by the grid rows.
fn field_label(ui: &mut Ui, label: &str, hint: &str, palette: Palette) {
    ui.vertical(|ui| {
        ui.set_max_width(LABEL_COLUMN_WIDTH - 8.0);
        ui.label(
            RichText::new(label)
                .size(theme::BODY_SIZE)
                .color(palette.text_secondary),
        );
        ui.label(
            RichText::new(hint)
                .size(theme::SMALL_SIZE)
                .color(palette.muted),
        );
    });
}

fn collision_label(policy: CollisionPolicy) -> &'static str {
    match policy {
        CollisionPolicy::KeepExisting => "Keep existing",
        CollisionPolicy::Rename => "Rename",
        CollisionPolicy::Overwrite => "Overwrite",
        CollisionPolicy::Skip => "Skip",
    }
}

fn collision_hint(policy: CollisionPolicy) -> &'static str {
    match policy {
        CollisionPolicy::KeepExisting => {
            "Leave the existing file untouched and report the download as failed."
        }
        CollisionPolicy::Rename => "Save alongside it as “name (1).ext”.",
        CollisionPolicy::Overwrite => "Replace the existing file.",
        CollisionPolicy::Skip => "Skip the download without touching the disk.",
    }
}

fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::Dark => "Dark",
        Theme::Light => "Light",
        Theme::System => "System (follow OS)",
    }
}

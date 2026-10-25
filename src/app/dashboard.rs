//! Dashboard: headline counters plus recent activity.

use egui::{Color32, RichText, Ui};

use crate::app::backend::Command;
use crate::app::{BulkDownloadApp, components, format, theme};
use crate::services::validation::redact_url;

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(ui, "Dashboard", "Queue health at a glance.");

    ui.horizontal(|ui| {
        if components::secondary_button(ui, "Refresh").clicked() {
            app.send(Command::RefreshDashboard);
        }
        if let Some(error) = &app.dashboard.error {
            ui.label(RichText::new(error).color(theme::ERROR));
        }
    });
    ui.add_space(6.0);

    let stats = app.dashboard.stats;
    ui.horizontal_wrapped(|ui| {
        stat_card(ui, "Queued", stats.queued.to_string(), theme::MUTED);
        stat_card(ui, "Active", stats.active.to_string(), theme::ACCENT);
        stat_card(ui, "Completed", stats.completed.to_string(), theme::SUCCESS);
        stat_card(ui, "Failed", stats.failed.to_string(), theme::ERROR);
        stat_card(
            ui,
            "Downloaded",
            format::format_bytes(Some(stats.bytes_downloaded_total)),
            theme::TEXT,
        );
    });

    ui.add_space(12.0);

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        components::section_title(ui, "Recent activity");
        ui.add_space(6.0);

        if app.dashboard.recent.is_empty() {
            components::empty_state(
                ui,
                "No downloads yet. Add some from the Add Downloads page.",
            );
            return;
        }

        egui::Grid::new("dashboard_recent")
            .num_columns(4)
            .spacing([16.0, 8.0])
            .striped(true)
            .show(ui, |ui| {
                for header in ["Name", "State", "Size", "Created"] {
                    ui.label(
                        RichText::new(header)
                            .color(theme::MUTED)
                            .size(12.0)
                            .strong(),
                    );
                }
                ui.end_row();

                for record in &app.dashboard.recent {
                    let name = ui.label(RichText::new(&record.display_name).color(theme::TEXT));
                    name.on_hover_text(redact_url(&record.source_url));

                    components::badge(
                        ui,
                        theme::state_label(record.state),
                        theme::state_color(record.state),
                    );
                    ui.label(
                        RichText::new(format::format_bytes(record.expected_size))
                            .color(theme::MUTED),
                    );
                    ui.label(
                        RichText::new(format::short_timestamp(&record.created_at))
                            .color(theme::MUTED)
                            .size(12.0),
                    );
                    ui.end_row();
                }
            });
    });
}

fn stat_card(ui: &mut Ui, label: &str, value: String, color: Color32) {
    components::card().show(ui, |ui| {
        ui.set_min_width(132.0);
        ui.label(RichText::new(label).color(theme::MUTED).size(12.0));
        ui.label(RichText::new(value).size(24.0).strong().color(color));
    });
}

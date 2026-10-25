//! History: paged, filtered, sortable list of settled downloads.

use egui::{RichText, Ui};

use crate::app::state::{Confirmation, HistoryState};
use crate::app::{BulkDownloadApp, components, format, theme};
use crate::database::repository::HISTORY_STATES;
use crate::models::download_status::DownloadState;
use crate::services::validation::redact_url;

/// Legacy empty-state copy.
const NO_HISTORY: &str = "No history yet. Finished, failed, cancelled and interrupted\ndownloads appear here automatically.";

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::page_heading(
        ui,
        "History",
        "Completed, failed, cancelled and interrupted downloads.",
    );

    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut app.history.search_input)
                .hint_text("Search by name or URL")
                .desired_width(280.0),
        );
        let submitted =
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));

        if components::secondary_button(ui, "Search").clicked() || submitted {
            app.history.apply_search();
            app.request_history();
        }

        let mut chosen = app
            .history
            .query
            .state
            .as_deref()
            .and_then(DownloadState::parse);
        let before = chosen;
        egui::ComboBox::from_id_salt("history_state")
            .selected_text(match chosen {
                Some(state) => theme::state_label(state),
                None => "All states",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut chosen, None, "All states");
                for state in HISTORY_STATES {
                    ui.selectable_value(&mut chosen, Some(state), theme::state_label(state));
                }
            });
        if chosen != before {
            app.history.set_state_filter(chosen);
            app.request_history();
        }

        if components::secondary_button(ui, "Refresh").clicked() {
            app.request_history();
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if components::danger_button(ui, "Clear history…").clicked() {
                app.confirm(Confirmation::ClearHistory);
            }
        });
    });

    if let Some(error) = &app.history.error {
        ui.add_space(4.0);
        ui.label(RichText::new(error).color(theme::ERROR));
    } else if app.history.loading {
        ui.add_space(4.0);
        components::muted(ui, "Loading…");
    }

    ui.add_space(8.0);

    let mut reload = false;
    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());

        if app.history.records.is_empty() && !app.history.loading {
            components::empty_state(ui, NO_HISTORY);
            return;
        }

        let records = app.history.records.clone();

        egui::Grid::new("history_rows")
            .num_columns(6)
            .spacing([14.0, 8.0])
            .striped(true)
            .show(ui, |ui| {
                reload |= sort_header(ui, &mut app.history, "Name", "display_name");
                ui.label(
                    RichText::new("State")
                        .color(theme::MUTED)
                        .size(12.0)
                        .strong(),
                );
                reload |= sort_header(ui, &mut app.history, "Downloaded", "downloaded_bytes");
                reload |= sort_header(ui, &mut app.history, "Size", "expected_size");
                reload |= sort_header(ui, &mut app.history, "Created", "created_at");
                reload |= sort_header(ui, &mut app.history, "Completed", "completed_at");
                ui.end_row();

                for record in &records {
                    let name = ui.label(RichText::new(&record.display_name).color(theme::TEXT));
                    name.on_hover_text(redact_url(&record.source_url));

                    components::badge(
                        ui,
                        theme::state_label(record.state),
                        theme::state_color(record.state),
                    );
                    ui.label(
                        RichText::new(format::format_bytes(Some(record.downloaded_bytes)))
                            .color(theme::TEXT),
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
                    ui.label(
                        RichText::new(
                            record
                                .completed_at
                                .as_deref()
                                .map(format::short_timestamp)
                                .unwrap_or_else(|| format::DASH.to_string()),
                        )
                        .color(theme::MUTED)
                        .size(12.0),
                    );
                    ui.end_row();
                }
            });
    });

    if reload {
        app.request_history();
    }

    ui.add_space(8.0);

    components::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let previous = components::secondary_button(ui, "◀ Previous");
            if previous.clicked() {
                app.history.previous_page();
                app.request_history();
            }
            components::muted(ui, format!("Page {}", app.history.page_number()));

            let has_next = app.history.has_next_page();
            ui.add_enabled_ui(has_next, |ui| {
                if components::secondary_button(ui, "Next ▶").clicked() {
                    app.history.next_page();
                    app.request_history();
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                components::muted(ui, format!("{} row(s)", app.history.records.len()));
            });
        });
        components::muted(
            ui,
            "Clear history deletes at most 100 records per pass; the app repeats until nothing is left.",
        );
    });
}

/// Render a clickable sortable column header; `true` when the order changed.
fn sort_header(ui: &mut Ui, history: &mut HistoryState, label: &str, column: &str) -> bool {
    let active = history.query.sort_by == column;
    let arrow = if !active {
        ""
    } else if history.query.sort_dir.eq_ignore_ascii_case("ASC") {
        " ▲"
    } else {
        " ▼"
    };
    let text = format!("{label}{arrow}");
    let response = ui.add(
        egui::Button::new(RichText::new(text).size(12.0).strong().color(if active {
            theme::TEXT
        } else {
            theme::MUTED
        }))
        .frame(false),
    );
    if response.clicked() {
        history.toggle_sort(column);
        true
    } else {
        false
    }
}

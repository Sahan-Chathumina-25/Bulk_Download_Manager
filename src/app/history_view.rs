//! History: paged, filtered, sortable list of settled downloads.

use egui::{RichText, Ui};

use crate::app::state::{Confirmation, HistoryState};
use crate::app::theme::{self, Palette};
use crate::app::{BulkDownloadApp, components, format};
use crate::database::repository::HISTORY_STATES;
use crate::models::diagnostic::host_from_url;
use crate::models::download_status::DownloadState;
use crate::services::validation::redact_url;

pub fn show(app: &mut BulkDownloadApp, ui: &mut Ui) {
    let palette = theme::palette(ui);
    components::page_heading(
        ui,
        "History",
        "Completed, failed, cancelled and interrupted downloads.",
    );

    toolbar(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    table(app, ui, palette);
    ui.add_space(theme::SPACE_2);
    pagination(app, ui);
}

/// Search, status filter, refresh and the destructive clear action.
fn toolbar(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    let mut commit_search = false;

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            let response = components::search_input(
                ui,
                &mut app.history.search_input,
                "Search by name or URL",
                300.0,
            );
            commit_search |=
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));

            if components::secondary_button(ui, "Search").clicked() {
                commit_search = true;
            }

            let mut chosen = app
                .history
                .query
                .state
                .as_deref()
                .and_then(DownloadState::parse);
            let before = chosen;
            egui::ComboBox::from_id_salt("history_state_filter")
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

        if let Some(search) = app.history.query.search.as_deref() {
            ui.add_space(theme::SPACE_1);
            components::muted(ui, format!("Searching for “{search}”."));
        }
        if let Some(state) = app
            .history
            .query
            .state
            .as_deref()
            .and_then(DownloadState::parse)
        {
            ui.add_space(theme::SPACE_1);
            components::muted(
                ui,
                format!("Showing {} rows only.", theme::state_label(state)),
            );
        }
    });

    if commit_search {
        app.history.apply_search();
        app.request_history();
    }

    if let Some(error) = &app.history.error {
        ui.add_space(theme::SPACE_1);
        ui.label(
            RichText::new(error)
                .size(theme::SMALL_SIZE)
                .color(palette.error),
        );
    }
}

/// Grid of settled downloads with sortable headers.
fn table(app: &mut BulkDownloadApp, ui: &mut Ui, palette: Palette) {
    let mut reload = false;
    let records = app.history.records.clone();

    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());

        if records.is_empty() {
            if app.history.loading {
                components::empty_state_action(
                    ui,
                    "Loading history…",
                    "Reading settled downloads from the local database.",
                    "",
                );
                return;
            }
            if app.history.error.is_some() {
                components::empty_state_action(
                    ui,
                    "History unavailable",
                    "The history query failed. Check the message above, then refresh.",
                    "",
                );
                return;
            }

            let has_search = app.history.query.search.is_some();
            let has_filter = app.history.query.state.is_some();
            let clicked = if has_search {
                components::empty_state_action(
                    ui,
                    "No matching history",
                    "No settled download matches the current search and filter.",
                    "Clear search",
                )
            } else if has_filter {
                components::empty_state_action(
                    ui,
                    "No history in this state",
                    "No settled download has the selected status. Show all states to see the rest.",
                    "Show all states",
                )
            } else {
                components::empty_state_action(
                    ui,
                    "No history yet",
                    "Completed, failed, cancelled and interrupted downloads appear here automatically.",
                    "",
                )
            };

            if clicked {
                if has_search {
                    app.history.search_input.clear();
                    app.history.apply_search();
                } else if has_filter {
                    app.history.set_state_filter(None);
                }
                app.request_history();
            }
            return;
        }

        egui::ScrollArea::horizontal()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("history_rows")
                    .num_columns(9)
                    .spacing([14.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        reload |= sort_header(ui, &mut app.history, "Name", "display_name", palette);
                        header(ui, "Host", palette);
                        reload |=
                            sort_header(ui, &mut app.history, "Status", "state", palette);
                        reload |= sort_header(
                            ui,
                            &mut app.history,
                            "Downloaded",
                            "downloaded_bytes",
                            palette,
                        );
                        reload |= sort_header(
                            ui,
                            &mut app.history,
                            "Size",
                            "expected_size",
                            palette,
                        );
                        reload |= sort_header(
                            ui,
                            &mut app.history,
                            "Started",
                            "created_at",
                            palette,
                        );
                        reload |= sort_header(
                            ui,
                            &mut app.history,
                            "Completed",
                            "completed_at",
                            palette,
                        );
                        reload |=
                            sort_header(ui, &mut app.history, "Retries", "retry_count", palette);
                        header(ui, "Error", palette);
                        ui.end_row();

                        for record in &records {
                            components::ellipsized_width(
                                ui,
                                RichText::new(&record.display_name).color(palette.text),
                                &redact_url(&record.source_url),
                                220.0,
                            );

                            let host = host_from_url(&record.source_url);
                            ui.label(
                                RichText::new(if host.is_empty() {
                                    format::DASH.to_string()
                                } else {
                                    host
                                })
                                .size(theme::SMALL_SIZE)
                                .color(palette.muted),
                            )
                            .on_hover_text(redact_url(&record.source_url));

                            components::status_badge(
                                ui,
                                theme::state_label(record.state),
                                palette.state_color(record.state),
                            );

                            ui.label(
                                RichText::new(format::format_bytes(Some(
                                    record.downloaded_bytes,
                                )))
                                .color(palette.text),
                            );
                            ui.label(
                                RichText::new(format::format_bytes(record.expected_size))
                                    .color(palette.muted),
                            );
                            ui.label(
                                RichText::new(format::short_timestamp(&record.created_at))
                                    .color(palette.muted)
                                    .size(theme::SMALL_SIZE),
                            );
                            ui.label(
                                RichText::new(
                                    record
                                        .completed_at
                                        .as_deref()
                                        .map(format::short_timestamp)
                                        .unwrap_or_else(|| format::DASH.to_string()),
                                )
                                .color(palette.muted)
                                .size(theme::SMALL_SIZE),
                            );
                            ui.label(
                                RichText::new(record.retry_count.to_string())
                                    .color(palette.muted)
                                    .size(theme::SMALL_SIZE),
                            );

                            components::ellipsized_width(
                                ui,
                                RichText::new(
                                    record.error_summary.as_deref().unwrap_or(format::DASH),
                                )
                                .color(if record.failure_category.is_some() {
                                    palette.error
                                } else {
                                    palette.muted
                                })
                                .size(theme::SMALL_SIZE),
                                record.error_summary.as_deref().unwrap_or_default(),
                                260.0,
                            );
                            ui.end_row();
                        }
                    });
            });
    });

    if reload {
        app.request_history();
    }
}

/// Non-sortable column header.
fn header(ui: &mut Ui, label: &str, palette: Palette) {
    ui.label(
        RichText::new(label)
            .size(theme::SMALL_SIZE)
            .strong()
            .color(palette.text_secondary),
    );
}

/// Render a clickable sortable column header; `true` when the order changed.
fn sort_header(
    ui: &mut Ui,
    history: &mut HistoryState,
    label: &str,
    column: &str,
    palette: Palette,
) -> bool {
    let active = history.query.sort_by == column;
    let arrow = if !active {
        ""
    } else if history.query.sort_dir.eq_ignore_ascii_case("ASC") {
        " ▲"
    } else {
        " ▼"
    };
    let text = format!("{label}{arrow}");
    let response = ui
        .add(
            egui::Button::new(RichText::new(text).size(theme::SMALL_SIZE).strong().color(
                if active {
                    palette.text
                } else {
                    palette.text_secondary
                },
            ))
            .frame(false),
        )
        .on_hover_text(format!("Sort by {label}"));
    if response.clicked() {
        history.toggle_sort(column);
        true
    } else {
        false
    }
}

/// Repository-backed paging controls.
fn pagination(app: &mut BulkDownloadApp, ui: &mut Ui) {
    components::card_for(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let can_go_back = app.history.query.offset > 0;
            ui.add_enabled_ui(can_go_back, |ui| {
                if components::secondary_button(ui, "◀ Previous").clicked() {
                    app.history.previous_page();
                    app.request_history();
                }
            });
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

//! History: search, sort, page through finished transfers.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Disableable as _, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{App, components, format, theme};
use crate::app::backend::{Command, QueueAction};
use crate::app::state::Confirmation;
use crate::models::download_status::DownloadState;

pub fn show(app: &App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(window, cx);
    let records = &app.history.records;

    let filters = [
        ("All", None),
        ("Completed", Some(DownloadState::Completed)),
        ("Failed", Some(DownloadState::Failed)),
        ("Cancelled", Some(DownloadState::Cancelled)),
        ("Interrupted", Some(DownloadState::Interrupted)),
    ];
    let current = app.history.query.state.as_deref();
    let filter_bar = div()
        .flex()
        .flex_wrap()
        .gap(px(6.0))
        .children(filters.iter().map(|(label, state)| {
            let active = match (current, state) {
                (None, None) => true,
                (Some(value), Some(expected)) => value == expected.as_str(),
                _ => false,
            };
            let label = *label;
            let state = *state;
            Button::new(format!("history-filter-{label}"))
                .label(label)
                .when(active, |button| button.primary())
                .when(!active, |button| button.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.history.set_state_filter(state);
                    this.request_history();
                    cx.notify();
                }))
        }));

    let header = div()
        .flex()
        .gap(px(8.0))
        .items_center()
        .child(
            div()
                .w(px(260.0))
                .child(Input::new(&app.history_search)),
        )
        .child(
            Button::new("history-search")
                .icon(IconName::Search)
                .label("Search")
                .on_click(cx.listener(|this, _, _, cx| {
                    let text = this.history_search.read(cx).value().to_string();
                    this.history.search_input = text;
                    this.history.apply_search();
                    this.request_history();
                    cx.notify();
                })),
        )
        .child(
            Button::new("history-refresh")
                .ghost()
                .label("Refresh")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.request_history();
                    cx.notify();
                })),
        );

    let rows = records.iter().map(|record| {
        let id = record.id.unwrap_or(-1);
        let checked = app.history.selected.contains(&id);
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .p(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        Checkbox::new(format!("history-select-{id}"))
                            .checked(checked)
                            .accessibility_label("Select history record")
                            .on_change(cx.listener(move |this, value, _, cx| {
                                if *value {
                                    this.history.selected.insert(id);
                                } else {
                                    this.history.selected.remove(&id);
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().child(record.display_name.clone()))
                    .child(components::badge(
                        theme::state_label(record.state),
                        theme::state_color(record.state, palette),
                        palette,
                    )),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(11.0))
                    .text_color(palette.text_muted)
                    .child(format::format_bytes(Some(record.downloaded_bytes)))
                    .child(record.completed_at.clone().unwrap_or_else(|| "—".into())),
            )
            .when_some(record.error_summary.clone(), |el, error| {
                el.child(components::muted(error, palette.danger))
            })
    });

    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(components::page_header(
            "History",
            "Search finished downloads and retry failures.",
        ))
        .child(header)
        .child(filter_bar)
        .children(rows)
        .when(records.is_empty(), |el| {
            el.child(components::empty_state(
                "No history yet",
                "Completed and failed downloads appear here.",
                palette,
            ))
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    Button::new("history-prev")
                        .ghost()
                        .label("Previous")
                        .disabled(app.history.query.offset == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history.previous_page();
                            this.request_history();
                            cx.notify();
                        })),
                )
                .child(components::muted(
                    format!("Page {}", app.history.page_number()),
                    palette.text_muted,
                ))
                .child(
                    Button::new("history-next")
                        .ghost()
                        .label("Next")
                        .disabled(!app.history.has_next_page())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history.next_page();
                            this.request_history();
                            cx.notify();
                        })),
                )
                .child(div().flex_1())
                .child(
                    Button::new("history-retry")
                        .label("Retry failed")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let ids: Vec<i64> = this
                                .history
                                .records
                                .iter()
                                .filter(|record| record.state == DownloadState::Failed)
                                .filter_map(|record| record.id)
                                .collect();
                            if !ids.is_empty() {
                                this.send(Command::QueueAction {
                                    ids,
                                    action: QueueAction::Retry,
                                });
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("history-delete-files")
                        .label("Also delete downloaded files")
                        .checked(app.history.keep_files_on_clear)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.history.keep_files_on_clear = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("history-clear")
                        .danger()
                        .label("Clear history…")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm(Confirmation::ClearHistory);
                            cx.notify();
                        })),
                ),
        )
}

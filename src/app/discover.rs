//! Discover: scan a page, review candidates, queue only what is selected.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Disableable as _, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{App, components, format, theme};
use crate::app::backend::Command;
use crate::services::validation::redact_url;

pub fn show(app: &App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(window, cx);
    let candidates = app.discovery.candidates();
    let visible = app.discovery.visible_indices();
    let selected = app.discovery.selected_count();

    let scan_bar = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(Input::new(&app.discovery_url).w(px(420.0)))
        .child(
            Button::new("discover-scan")
                .primary()
                .icon(IconName::Search)
                .label("Scan page")
                .disabled(app.discovery.scanning)
                .on_click(cx.listener(|this, _, _, cx| {
                    let url = this.discovery_url.read(cx).value().to_string();
                    this.discovery.page_url = url.clone();
                    this.discovery.scanning = true;
                    this.send(Command::ScanPage { url });
                    cx.notify();
                })),
        )
        .child(
            Button::new("discover-cancel")
                .ghost()
                .label("Cancel")
                .disabled(!app.discovery.scanning)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.send(Command::CancelScan);
                    this.discovery.scanning = false;
                    cx.notify();
                })),
        );

    let rows = visible.iter().map(|index| {
        let index = *index;
        let candidate = &candidates[index];
        let checked = app.discovery.is_selected(index);
        let selectable = candidate.filtered_reason.is_none();
        let label = if candidate.is_likely_file {
            "Direct file"
        } else {
            "Web page"
        };
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
                        Checkbox::new(format!("discover-row-{index}"))
                            .checked(checked && selectable)
                            .accessibility_label(format!("Include {}", candidate.filename))
                            .disabled(!selectable)
                            .on_change(cx.listener(move |this, _, _, cx| {
                                this.discovery.toggle(index);
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().child(candidate.filename.clone()))
                    .child(components::badge(label, palette.accent, palette)),
            )
            .child(components::muted(
                format!(
                    "{} · {} · {}",
                    candidate.extension.clone().unwrap_or_else(|| "—".into()),
                    format::format_bytes(candidate.size),
                    candidate
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "—".into())
                ),
                palette.text_muted,
            ))
            .child(components::muted(
                redact_url(&candidate.candidate_url),
                palette.text_muted,
            ))
            .when_some(candidate.filtered_reason.clone(), |el, reason| {
                el.child(components::muted(reason, palette.danger))
            })
    });

    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(components::page_header(
            "Discover",
            "Scan a page, review links, then add only what you choose.",
        ))
        .child(scan_bar)
        .child(components::muted(
            app.discovery.status.clone(),
            palette.text_muted,
        ))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    Checkbox::new("discover-unknown")
                        .label("Include unknown types")
                        .checked(app.discovery.include_unknown)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.discovery.include_unknown = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("discover-all")
                        .ghost()
                        .label("Select all")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.discovery.set_all(true);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("discover-none")
                        .ghost()
                        .label("Deselect all")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.discovery.set_all(false);
                            cx.notify();
                        })),
                ),
        )
        .children(rows)
        .when(candidates.is_empty(), |el| {
            el.child(components::empty_state(
                "No scan results",
                "Enter a page URL and choose Scan page.",
                palette,
            ))
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    Button::new("discover-destination")
                        .icon(IconName::Folder)
                        .label("Destination")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                this.discovery_destination = Some(folder);
                                cx.notify();
                            }
                        })),
                )
                .child(components::muted(
                    destination_label(app),
                    palette.text_muted,
                ))
                .child(div().flex_1())
                .child(
                    Button::new("discover-enqueue")
                        .primary()
                        .label(format!("Add {selected} selected"))
                        .disabled(selected == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let destination = this
                                .discovery_destination
                                .clone()
                                .or_else(|| {
                                    let configured =
                                        this.settings.default_destination_dir.trim();
                                    (!configured.is_empty())
                                        .then(|| std::path::PathBuf::from(configured))
                                })
                                .unwrap_or_else(|| std::path::PathBuf::from("."));
                            let candidates = this.discovery.candidates_for_planner();
                            this.send(Command::EnqueueCandidates {
                                candidates,
                                destination,
                            });
                            cx.notify();
                        })),
                ),
        )
}

fn destination_label(app: &App) -> String {
    app.discovery_destination
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            let configured = app.settings.default_destination_dir.trim();
            if configured.is_empty() {
                "Default download folder".to_string()
            } else {
                configured.to_string()
            }
        })
}

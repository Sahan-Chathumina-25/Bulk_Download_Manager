//! Add Downloads: paste URLs, review them, then queue them.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::{Disableable as _, IconName};
use gpui_kit::*;

use super::{App, components, theme};
use crate::app::backend::Command;

pub fn show(app: &App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(window, cx);
    let single_url = app.add_line_count == 1;

    let form = div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(components::section_title("Download URLs", palette.text))
        .child(
            div()
                .rounded(px(10.0))
                .border_1()
                .border_color(palette.border)
                .bg(palette.surface_alt)
                .child(Textarea::new(&app.add_urls).h(px(150.0))),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    Button::new("add-pick-folder")
                        .icon(IconName::Folder)
                        .label("Destination")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                this.add.destination = Some(folder);
                                cx.notify();
                            }
                        })),
                )
                .child(components::muted(destination_label(app), palette.text_muted)),
        )
        .child(
            div()
                .flex()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(components::muted("File name (one URL only)", palette.text_muted))
                        .child(Input::new(&app.add_filename).disabled(!single_url)),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(components::muted("SHA-256 (one URL only)", palette.text_muted))
                        .child(Input::new(&app.add_checksum).disabled(!single_url)),
                ),
        )
        .child(
            Checkbox::new("add-start")
                .label("Start downloads immediately")
                .checked(app.add.start_immediately)
                .on_change(cx.listener(|this, value, _, cx| {
                    this.add.start_immediately = *value;
                    cx.notify();
                })),
        )
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(
                    Button::new("add-preview")
                        .label("Preview")
                        .disabled(app.add.preview_pending)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let text = this.add_urls.read(cx).value().to_string();
                            this.add.preview_pending = true;
                            this.send(Command::PreviewUrls { text });
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("add-submit")
                        .primary()
                        .icon(IconName::Plus)
                        .label("Add to queue")
                        .disabled(app.add.add_pending)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.add.add_pending = true;
                            let text = this.add_urls.read(cx).value().to_string();
                            this.add_line_count = count_lines(&text);
                            let single = this.add_line_count == 1;
                            let filename = if single {
                                let value = this.add_filename.read(cx).value().trim().to_string();
                                (!value.is_empty()).then_some(value)
                            } else {
                                None
                            };
                            let checksum = if single {
                                let value = this.add_checksum.read(cx).value().trim().to_string();
                                (!value.is_empty()).then_some(value)
                            } else {
                                None
                            };
                            let destination = this.add.effective_destination(&this.settings);
                            let start_now = this.add.start_immediately;
                            this.send(Command::AddUrls {
                                text,
                                destination,
                                filename,
                                checksum,
                                start_now,
                            });
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("add-import")
                        .label("Import TXT…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Text", &["txt"])
                                .pick_file()
                            {
                                match std::fs::read_to_string(&path) {
                                    Ok(text) => {
                                        this.add_urls.update(cx, |state, cx| {
                                            state.set_value(&text, window, cx);
                                        });
                                        this.add_line_count = count_lines(&text);
                                    }
                                    Err(error) => this.toast(
                                        super::state::Severity::Error,
                                        format!("Could not read {}: {error}", path.display()),
                                    ),
                                }
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("add-clear")
                        .ghost()
                        .label("Clear")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_urls.update(cx, |state, cx| {
                                state.set_value("", window, cx);
                            });
                            this.add.clear_feedback();
                            this.add_line_count = 0;
                            cx.notify();
                        })),
                ),
        );

    let mut column = div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(components::page_header(
            "Add Downloads",
            "Paste URLs, review them, then add them to the queue.",
        ))
        .child(
            components::card(form, palette)
        );

    if let Some(error) = &app.add.error {
        column = column.child(
            div()
                .text_size(px(12.0))
                .text_color(palette.danger)
                .child(error.clone()),
        );
    }

    if let Some(preview) = &app.add.preview {
        column = column.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(components::section_title("Preview", palette.text))
                .child(components::muted(
                    format!(
                        "{} accepted · {} duplicate(s) · {} rejected",
                        preview.accepted.len(),
                        preview.duplicates.len(),
                        preview.rejected.len()
                    ),
                    palette.text_muted,
                ))
                .children(preview.rejected.iter().take(5).map(|(line, reason)| {
                    components::muted(format!("{line}: {reason}"), palette.danger)
                })),
        );
    }

    if let Some(outcome) = &app.add.outcome {
        column = column.child(components::muted(
            format!(
                "Added {} download(s); {} failed.",
                outcome.added,
                outcome.failed.len()
            ),
            palette.text_muted,
        ));
    }

    column
}

fn destination_label(app: &App) -> String {
    match app.add.effective_destination(&app.settings) {
        Some(path) => path.to_string_lossy().into_owned(),
        None => "Default download folder".to_string(),
    }
}

fn count_lines(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .count()
}

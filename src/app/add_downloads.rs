//! Add Downloads: paste URLs, review them, then queue them.

use dioxus::prelude::*;

use crate::app::{components, state::Severity, use_ui};
use crate::app::backend::Command;

#[component]
pub fn AddDownloads() -> Element {
    let mut ui = use_ui();

    let (urls, filename, checksum, destination, single_url, start_now, pending, previewing, error, preview, outcome) = {
        let model = ui.model.read();
        (
            model.add_urls.clone(),
            model.add_filename.clone(),
            model.add_checksum.clone(),
            destination_label(&model.settings.default_destination_dir, model.add.destination.as_ref()),
            model.add_line_count == 1,
            model.add.start_immediately,
            model.add.add_pending,
            model.add.preview_pending,
            model.add.error.clone(),
            model.add.preview.as_ref().map(|preview| (
                preview.accepted.len(),
                preview.duplicates.len(),
                preview.rejected.len(),
                preview.rejected.iter().take(5)
                    .map(|(line, reason)| format!("{line}: {reason}"))
                    .collect::<Vec<_>>(),
            )),
            model.add.outcome.clone(),
        )
    };

    rsx! {
        components::PageHeader {
            title: "Add Downloads".to_string(),
            subtitle: "Paste URLs, review them, then add them to the queue.".to_string(),
        }
        components::Card {
            section { class: "column",
                components::SectionTitle { text: "Download URLs".to_string(), tone: "".to_string() }
                textarea {
                    value: "{urls}",
                    placeholder: "One download URL per line…",
                    oninput: move |event| {
                        let value = event.value();
                        let mut model = ui.model.write();
                        model.add_line_count = count_lines(&value);
                        model.add_urls = value;
                    },
                }
                div { class: "row",
                    button {
                        onclick: move |_| {
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                ui.model.write().add.destination = Some(folder);
                            }
                        },
                        "Destination"
                    }
                    span { class: "muted", "{destination}" }
                }
                div { class: "row",
                    div { class: "field grow",
                        label { "File name (one URL only)" }
                        input {
                            value: "{filename}",
                            disabled: !single_url,
                            placeholder: "Optional file name",
                            oninput: move |event| ui.model.write().add_filename = event.value(),
                        }
                    }
                    div { class: "field grow",
                        label { "SHA-256 (one URL only)" }
                        input {
                            value: "{checksum}",
                            disabled: !single_url,
                            placeholder: "sha256:<hex>",
                            oninput: move |event| ui.model.write().add_checksum = event.value(),
                        }
                    }
                }
                label { class: "checkbox",
                    input {
                        r#type: "checkbox",
                        checked: start_now,
                        onchange: move |event| ui.model.write().add.start_immediately = event.checked(),
                    }
                    "Start downloads immediately"
                }
                div { class: "row",
                    button {
                        disabled: previewing,
                        onclick: move |_| {
                            let mut model = ui.model.write();
                            model.add.preview_pending = true;
                            let text = model.add_urls.clone();
                            model.send(Command::PreviewUrls { text });
                        },
                        "Preview"
                    }
                    button {
                        class: "primary",
                        disabled: pending,
                        onclick: move |_| submit(ui),
                        "Add to queue"
                    }
                    button {
                        onclick: move |_| import_txt(ui),
                        "Import TXT…"
                    }
                    button {
                        class: "ghost",
                        onclick: move |_| {
                            let mut model = ui.model.write();
                            model.add_urls.clear();
                            model.add_line_count = 0;
                            model.add.clear_feedback();
                        },
                        "Clear"
                    }
                }
            }
        }
        if let Some(error) = error {
            div { class: "danger", "{error}" }
        }
        if let Some((accepted, duplicates, rejected, reasons)) = preview {
            section { class: "column",
                components::SectionTitle { text: "Preview".to_string(), tone: "".to_string() }
                components::Muted {
                    text: format!("{accepted} accepted · {duplicates} duplicate(s) · {rejected} rejected"),
                    tone: "".to_string(),
                }
                for reason in reasons {
                    components::Muted { text: reason, tone: "danger".to_string() }
                }
            }
        }
        if let Some(outcome) = outcome {
            components::Muted {
                text: format!("Added {} download(s); {} failed.", outcome.added, outcome.failed.len()),
                tone: "".to_string(),
            }
        }
    }
}

fn submit(mut ui: crate::app::Ui) {
    let mut model = ui.model.write();
    let text = model.add_urls.clone();
    let single = count_lines(&text) == 1;
    let filename = single.then(|| model.add_filename.trim().to_string()).filter(|value| !value.is_empty());
    let checksum = single.then(|| model.add_checksum.trim().to_string()).filter(|value| !value.is_empty());
    let destination = model.add.effective_destination(&model.settings);
    let start_now = model.add.start_immediately;
    model.add_line_count = count_lines(&text);
    model.add.add_pending = true;
    model.send(Command::AddUrls {
        text,
        destination,
        filename,
        checksum,
        start_now,
    });
}

fn import_txt(mut ui: crate::app::Ui) {
    let Some(path) = rfd::FileDialog::new().add_filter("Text", &["txt"]).pick_file() else {
        return;
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let mut model = ui.model.write();
            model.add_line_count = count_lines(&text);
            model.add_urls = text;
        }
        Err(error) => ui.model.write().toast(
            Severity::Error,
            format!("Could not read {}: {error}", path.display()),
        ),
    }
}

fn destination_label(configured: &str, selected: Option<&std::path::PathBuf>) -> String {
    match selected {
        Some(path) => path.to_string_lossy().into_owned(),
        None if configured.trim().is_empty() => "Default download folder".to_string(),
        None => configured.to_string(),
    }
}

fn count_lines(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .count()
}

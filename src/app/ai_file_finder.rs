//! AI File Finder: offline, advisory archive detection over pasted URLs.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Textarea;
use gpui_kit::component::{Disableable as _, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{App, components, format, theme};
use crate::app::backend::Command;
use crate::services::validation::redact_url;

pub fn show(app: &App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let palette = theme::palette(window, cx);
    let candidates = app.ai.candidates();
    let selected = app.ai.selected_count();

    let scan_bar = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .rounded(px(10.0))
                .border_1()
                .border_color(palette.border)
                .bg(palette.surface_alt)
                .child(Textarea::new(&app.ai_urls).h(px(100.0))),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(12.0))
                .child(
                    Checkbox::new("ai-from-discover")
                        .label("Use links from the last Discover scan")
                        .checked(app.ai.analyze_discovered)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.ai.analyze_discovered = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("ai-probe")
                        .label("Verify with a small HTTP check")
                        .checked(app.ai.probe_metadata)
                        .on_change(cx.listener(|this, value, _, cx| {
                            this.ai.probe_metadata = *value;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("ai-find")
                        .primary()
                        .icon(IconName::Search)
                        .label("Find files")
                        .disabled(app.ai.analyzing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai.analyzing = true;
                            if this.ai.analyze_discovered {
                                let candidates = this.discovery.candidates_for_planner();
                                this.send(Command::AnalyzeCandidates {
                                    candidates,
                                    probe: this.ai.probe_metadata,
                                });
                            } else {
                                let text = this.ai_urls.read(cx).value().to_string();
                                this.ai.urls_text = text.clone();
                                this.send(Command::AnalyzeUrls {
                                    text,
                                    probe: this.ai.probe_metadata,
                                });
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("ai-cancel")
                        .ghost()
                        .label("Cancel")
                        .disabled(!app.ai.analyzing)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.send(Command::CancelAnalysis);
                            this.ai.analyzing = false;
                            cx.notify();
                        })),
                ),
        )
        .child(components::muted(
            app.ai.status.clone(),
            palette.text_muted,
        ));

    let rows = candidates.iter().enumerate().map(|(index, candidate)| {
        let checked = app.ai.is_selected(index);
        let open = app.ai.expanded.contains(&index);
        let color = if candidate.score >= 75 {
            palette.success
        } else if candidate.score >= 50 {
            palette.accent
        } else {
            palette.text_muted
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
                        Checkbox::new(format!("ai-select-{index}"))
                            .checked(checked)
                            .accessibility_label(format!("Include {}", candidate.filename))
                            .on_change(cx.listener(move |this, _, _, cx| {
                                this.ai.toggle(index);
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().child(candidate.filename.clone()))
                    .child(components::badge(
                        format!("{} · {}", candidate.score, candidate.status),
                        color,
                        palette,
                    )),
            )
            .child(components::muted(
                format!(
                    "{} · confidence {} · {}",
                    candidate.archive_type,
                    candidate.confidence.label(),
                    format::format_bytes(candidate.size)
                ),
                palette.text_muted,
            ))
            .child(components::muted(
                redact_url(&candidate.url),
                palette.text_muted,
            ))
            .child(
                Button::new(format!("ai-evidence-{index}"))
                    .ghost()
                    .label(if open { "Hide evidence" } else { "View evidence" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.ai.expanded.remove(&index) {
                            this.ai.expanded.insert(index);
                        }
                        cx.notify();
                    })),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(candidate.evidence.iter().map(|item| {
                            components::muted(
                                format!("{:+} {}", item.weight, item.reason),
                                palette.text_muted,
                            )
                        }))
                        .children(candidate.missing_evidence.iter().map(|item| {
                            components::muted(format!("Missing: {item}"), palette.warning)
                        })),
                )
            })
    });

    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(components::page_header(
            "AI File Finder",
            "Offline archive suggestions — verify before adding anything.",
        ))
        .child(scan_bar)
        .when_some(app.ai.report.as_ref(), |el, report| {
            el.child(
                div()
                    .flex()
                    .gap(px(10.0))
                    .child(components::muted(
                        format!("{} analyzed", report.analyzed_count),
                        palette.text,
                    ))
                    .child(components::muted(
                        format!("{} candidates", report.candidate_count),
                        palette.text,
                    ))
                    .child(components::muted(
                        format!("{} filtered", report.filtered_count),
                        palette.text_muted,
                    )),
            )
        })
        .children(rows)
        .when(candidates.is_empty(), |el| {
            el.child(components::empty_state(
                "No analysis yet",
                "Paste URLs or reuse the last Discover scan, then choose Find files.",
                palette,
            ))
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    Button::new("ai-all")
                        .ghost()
                        .label("Select all")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai.set_all(true);
                            cx.notify();
                        })),
                )
                .child(components::muted(
                    if app.ai.selected_has_incomplete_groups() {
                        "Some selected files belong to incomplete archive groups."
                    } else {
                        "Selections are advisory only."
                    },
                    if app.ai.selected_has_incomplete_groups() {
                        palette.warning
                    } else {
                        palette.text_muted
                    },
                ))
                .child(div().flex_1())
                .child(
                    Button::new("ai-enqueue")
                        .primary()
                        .label(format!("Add {selected} selected"))
                        .disabled(selected == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let destination = this
                                .ai_destination
                                .clone()
                                .or_else(|| {
                                    let configured = this.settings.default_destination_dir.trim();
                                    (!configured.is_empty())
                                        .then(|| std::path::PathBuf::from(configured))
                                })
                                .unwrap_or_else(|| std::path::PathBuf::from("."));
                            let candidates = this.ai.selected_for_planner();
                            this.send(Command::EnqueueAiCandidates {
                                candidates,
                                destination,
                            });
                            cx.notify();
                        })),
                ),
        )
}

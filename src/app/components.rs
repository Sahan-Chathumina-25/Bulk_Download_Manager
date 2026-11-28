//! Small Dioxus building blocks shared by the page views.

use dioxus::prelude::*;

/// Page heading with a short, task-oriented subtitle.
#[component]
pub fn PageHeader(title: String, subtitle: String) -> Element {
    rsx! {
        header { class: "page-header",
            h1 { "{title}" }
            p { "{subtitle}" }
        }
    }
}

/// A short section label. `tone` maps to a CSS color class.
#[component]
pub fn SectionTitle(text: String, tone: String) -> Element {
    rsx! {
        div { class: "section-title {tone}", "{text}" }
    }
}

/// Secondary text.
#[component]
pub fn Muted(text: String, tone: String) -> Element {
    rsx! {
        div { class: "muted {tone}", "{text}" }
    }
}

/// Elevated surface with a hairline border.
#[component]
pub fn Card(children: Element) -> Element {
    rsx! {
        section { class: "card", {children} }
    }
}

/// A status pill. The text label is always shown beside the color.
#[component]
pub fn Badge(text: String, tone: String) -> Element {
    rsx! {
        span { class: "badge {tone}", "{text}" }
    }
}

/// Determinate or indeterminate progress bar.
#[component]
pub fn ProgressBar(fraction: Option<f32>) -> Element {
    let (class, width) = match fraction {
        Some(value) => ("progress", format!("{}%", (value.clamp(0.0, 1.0) * 100.0) as u32)),
        None => ("progress indeterminate", "35%".to_string()),
    };
    rsx! {
        div { class,
            span { style: "width: {width};" }
        }
    }
}

/// Empty-state prompt with an explicit next action described in the subtitle.
#[component]
pub fn EmptyState(title: String, subtitle: String) -> Element {
    rsx! {
        div { class: "empty",
            strong { "{title}" }
            span { "{subtitle}" }
        }
    }
}

//! Dioxus desktop UI layer.
//!
//! The UI is split from the download engine: every side effect goes through
//! [`backend::AppBackend`], and the root component renders the state pushed
//! back through `UiEvent`s on a short timer.

use std::sync::{Arc, Mutex, mpsc::Receiver};

use dioxus::prelude::*;
use futures_timer::Delay;

use crate::app::backend::{AppBackend, Startup, UiEvent};
use crate::app::model::{TICK_INTERVAL, UiModel};
use crate::app::state::{Confirmation, Page};

pub mod add_downloads;
pub mod ai_file_finder;
pub mod backend;
pub mod components;
pub mod dashboard;
pub mod discover;
pub mod format;
pub mod history_view;
pub mod model;
pub mod queue_view;
pub mod settings_view;
pub mod state;
pub mod theme;

/// Opaque backend handle for component props; identity is irrelevant to render.
#[derive(Clone)]
pub struct BackendHandle(pub Arc<AppBackend>);

impl PartialEq for BackendHandle {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// Opaque event receiver for component props.
#[derive(Clone)]
pub struct EventsHandle(pub Arc<Mutex<Receiver<UiEvent>>>);

impl PartialEq for EventsHandle {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// Opaque startup snapshot for component props.
#[derive(Clone)]
pub struct StartupHandle(pub Arc<Startup>);

impl PartialEq for StartupHandle {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// Context shared by every page component.
#[derive(Clone, Copy)]
pub struct Ui {
    pub model: Signal<UiModel>,
}

/// Look up the UI context provided by [`App`].
pub fn use_ui() -> Ui {
    try_use_context::<Ui>().expect("Ui context missing; <App> must be the root component")
}

/// Root component: owns the model, pumps backend events, and draws the shell.
#[component]
pub fn App(backend: BackendHandle, startup: StartupHandle, events: EventsHandle) -> Element {
    let mut ui = use_context_provider(|| Ui {
        model: Signal::new(UiModel::new(
            Arc::clone(&backend.0),
            Arc::clone(&startup.0),
            Arc::clone(&events.0),
        )),
    });

    use_future(move || async move {
        loop {
            Delay::new(TICK_INTERVAL).await;
            ui.model.write().tick();
        }
    });

    let (page, active, speed, connected) = {
        let model = ui.model.read();
        (
            model.page,
            model.active_count(),
            format_speed_label(model.total_speed()),
            model.events_connected,
        )
    };

    let nav: Vec<(Page, &'static str)> = Page::ALL
        .iter()
        .map(|page| (*page, page.title()))
        .collect();

    let toasts: Vec<(String, String)> = {
        let model = ui.model.read();
        model
            .notifications
            .items()
            .iter()
            .map(|notification| {
                (
                    theme::severity_class(notification.severity).to_string(),
                    notification.text.clone(),
                )
            })
            .collect()
    };

    let confirmation = ui.model.read().confirmation.clone();

    rsx! {
        document::Stylesheet { href: asset!("/assets/main.css") }
        div { id: "main",
            nav { class: "nav",
                div { class: "brand", "Bulk Download Manager" }
                for (target, label) in nav {
                    button {
                        class: if target == page { "nav-link active" } else { "nav-link" },
                        onclick: move |_| ui.model.write().switch_page(target),
                        "{label}"
                    }
                }
                div { class: "nav-spacer" }
                div {
                    class: if connected { "connection" } else { "connection offline" },
                    if connected { "Engine connected" } else { "Engine disconnected" }
                }
            }
            div { class: "workspace",
                header { class: "toolbar",
                    div { class: "toolbar-title", "{page.title()}" }
                    div { class: "toolbar-stats",
                        span { "{active} active" }
                        span { "{speed}" }
                        button {
                            class: "ghost",
                            onclick: move |_| {
                                if ui.model.write().request_quit() {
                                    dioxus::desktop::window().close();
                                }
                            },
                            "Quit"
                        }
                    }
                }
                main { class: "page",
                    match page {
                        Page::Dashboard => rsx! { dashboard::Dashboard {} },
                        Page::Queue => rsx! { queue_view::Queue {} },
                        Page::AddDownloads => rsx! { add_downloads::AddDownloads {} },
                        Page::History => rsx! { history_view::History {} },
                        Page::Discover => rsx! { discover::Discover {} },
                        Page::Settings => rsx! { settings_view::Settings {} },
                        Page::AiFinder => rsx! { ai_file_finder::AiFinder {} },
                    }
                }
            }
            div { class: "toasts",
                for (tone, text) in toasts {
                    div { class: "toast {tone}", "{text}" }
                }
            }
            if let Some(pending) = confirmation {
                ConfirmationDialog { pending }
            }
        }
    }
}

#[component]
fn ConfirmationDialog(pending: Confirmation) -> Element {
    let mut ui = use_ui();
    let title = pending.title().to_string();
    let message = pending.message();
    let accept = pending.accept_label().to_string();
    let dangerous = pending.is_destructive();

    rsx! {
        div { class: "scrim",
            div { class: "modal",
                h2 { "{title}" }
                p { "{message}" }
                div { class: "row end",
                    button {
                        class: "ghost",
                        onclick: move |_| ui.model.write().confirmation = None,
                        "Cancel"
                    }
                    button {
                        class: if dangerous { "danger" } else { "primary" },
                        onclick: move |_| {
                            let pending = ui.model.write().confirmation.take();
                            if let Some(pending) = pending
                                && ui.model.write().accept_confirmation(pending)
                            {
                                dioxus::desktop::window().close();
                            }
                        },
                        "{accept}"
                    }
                }
            }
        }
    }
}

fn format_speed_label(speed: f64) -> String {
    if speed <= 0.0 {
        "—".to_string()
    } else {
        format::format_speed(Some(speed))
    }
}

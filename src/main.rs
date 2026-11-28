//! Application entry point: logging, backend startup, native Dioxus window.
//!
//! All I/O lives behind [`app::backend::AppBackend`]; this file only wires the
//! pieces together and owns the process lifetime.

// Hide the console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod database;
#[allow(dead_code)]
mod engine;
#[allow(dead_code)]
mod errors;
#[allow(dead_code)]
mod file_intelligence;
#[allow(dead_code)]
mod models;
#[allow(dead_code)]
mod services;

use std::sync::{Arc, Mutex, OnceLock, mpsc};

use dioxus::prelude::*;

use crate::app::{App, BackendHandle, EventsHandle, StartupHandle};
use crate::app::backend::{AppBackend, BackendConfig, Startup, UiEvent, default_database_path};

struct LaunchContext {
    backend: Arc<AppBackend>,
    startup: Arc<Startup>,
    events: Arc<Mutex<mpsc::Receiver<UiEvent>>>,
}

static LAUNCH_CONTEXT: OnceLock<LaunchContext> = OnceLock::new();

fn main() {
    init_tracing();

    let (events_tx, events_rx) = mpsc::channel();
    let (backend, startup) = match AppBackend::start(
        BackendConfig {
            database_path: default_database_path(),
        },
        events_tx,
    ) {
        Ok(started) => started,
        Err(error) => {
            tracing::error!(%error, "backend startup failed");
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Bulk Download Manager")
                .set_description(format!("The application could not start.\n\n{error}"))
                .show();
            std::process::exit(1);
        }
    };

    assert!(
        LAUNCH_CONTEXT
            .set(LaunchContext {
                backend: Arc::new(backend),
                startup: Arc::new(startup),
                events: Arc::new(Mutex::new(events_rx)),
            })
            .is_ok(),
        "launch context must be initialised once"
    );

    let window = dioxus::desktop::WindowBuilder::new()
        .with_title("Bulk Download Manager")
        .with_inner_size(dioxus::desktop::LogicalSize::new(1240, 780))
        .with_min_inner_size(dioxus::desktop::LogicalSize::new(1000, 650));
    let config = dioxus::desktop::Config::new().with_window(window);

    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(root);
}

/// Dioxus takes a plain `fn() -> Element`; handles live in the launch context.
fn root() -> Element {
    let context = LAUNCH_CONTEXT
        .get()
        .expect("launch context must be initialised before rendering");
    rsx! {
        App {
            backend: BackendHandle(Arc::clone(&context.backend)),
            startup: StartupHandle(Arc::clone(&context.startup)),
            events: EventsHandle(Arc::clone(&context.events)),
        }
    }
}

/// Initialise structured logging; `RUST_LOG` overrides the default filter.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,bulk_download_manager=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

//! Application entry point: logging, backend startup, native window.
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

use std::sync::mpsc;

use gpui_kit::*;

use crate::app::App;
use crate::app::backend::{AppBackend, BackendConfig, default_database_path};

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

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);

            let bounds = Bounds::centered(
                None,
                size(px(1240.0), px(780.0)),
                cx,
            );
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Bulk Download Manager".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            open_window(options, cx, |window, cx| {
                cx.new(|view_cx| App::new(backend, startup, events_rx, window, view_cx))
            })
            .expect("failed to open main window");
        });
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

//! Application entry point: logging, backend startup, native window.
//!
//! All I/O lives behind [`app::backend::AppBackend`]; this file only wires the
//! pieces together and owns the process lifetime.

// Hide the console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
// These layers are complete and unit-tested, but parts of their public surface
// (validation entry points, retry classification, database inspection helpers)
// are not reachable from the UI yet. Keep the allowance per module instead of
// disabling the lint for the whole crate.
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod database;
#[allow(dead_code)]
mod engine;
#[allow(dead_code)]
mod errors;
#[allow(dead_code)]
mod models;
#[allow(dead_code)]
mod services;

use std::sync::mpsc;

use crate::app::BulkDownloadApp;
use crate::app::backend::{AppBackend, BackendConfig, UiEvent, default_database_path};

fn main() -> eframe::Result {
    init_tracing();

    let (events_tx, events_rx) = mpsc::channel::<UiEvent>();
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Bulk Download Manager")
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([1000.0, 650.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Bulk Download Manager",
        options,
        Box::new(move |cc| {
            Ok(Box::new(BulkDownloadApp::new(
                cc, backend, startup, events_rx,
            )))
        }),
    )
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

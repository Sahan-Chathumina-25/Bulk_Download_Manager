//! Minimal scaffold for the Rust rewrite of Bulk Download Manager.
//!
//! This validates the toolchain and the eframe 0.37 API before the full
//! application is ported. The real application grows out of this file.

// Hide the console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Core modules ported so far; the app wiring consumes them in later phases.
#![allow(dead_code)]

mod config;
mod database;
mod errors;
mod models;
mod services;

fn main() -> eframe::Result {
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
        Box::new(|_cc| Ok(Box::new(ScaffoldApp::default()))),
    )
}

/// The root application. Phase 2 scaffold: one panel and one button.
#[derive(Default)]
struct ScaffoldApp {
    clicks: u32,
}

impl eframe::App for ScaffoldApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Bulk Download Manager");
            ui.add_space(8.0);
            ui.label("Rust scaffold - the Python implementation is being ported.");
            ui.add_space(8.0);
            if ui.button("Click me").clicked() {
                self.clicks += 1;
            }
            ui.label(format!("Clicked {} time(s)", self.clicks));
        });
    }
}

//! LAN Audio Share: point-to-point audio streaming over a local network.
//!
//! Launch it and it resumes what it was doing last time. No server, no session
//! to join, nothing to unmute.

// Do not pop a console window alongside the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod devices;
mod engine;
mod net;
mod protocol;
mod receiver;
mod resample;
mod sender;
mod stats;
mod ui;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LAN Audio Share")
            .with_inner_size([470.0, 600.0])
            .with_min_inner_size([420.0, 440.0]),
        ..Default::default()
    };

    eframe::run_native(
        "LAN Audio Share",
        options,
        Box::new(|_cc| Ok(Box::new(ui::App::new()))),
    )
}

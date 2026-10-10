#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod document;
mod export;
mod history;
mod mesh;
mod paint;
mod project;
mod renderer;
mod spp;

use anyhow::Result;

fn main() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Kalium • Artist-first 3D Painter")
            .with_inner_size([1430.0, 860.0])
            .with_min_inner_size([900.0, 600.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "Kalium",
        options,
        Box::new(|cc| Ok(Box::new(app::KaliumApp::new(cc)))),
    )
    .map_err(|err| anyhow::anyhow!("Cannot start Kalium: {err}"))
}

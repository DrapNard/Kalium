// Keep a console attached even for release builds: Kalium includes a headless CLI.
// A GUI-only Windows launcher can be added as a separate binary later.

mod app;
#[allow(dead_code)] // High-to-low projection awaits a persisted mesh-alignment contract.
mod bake;
mod brush_preset;
mod cli;
mod document;
mod export;
#[cfg(feature = "gpu-experimental")]
mod gpu_brush;
mod history;
mod layout;
mod mesh;
mod ops;
mod paint;
mod project;
#[allow(dead_code)]
mod recovery;
mod renderer;
mod spp;
mod uv;

use anyhow::Result;

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if !arguments.is_empty() {
        return cli::run(&arguments);
    }
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

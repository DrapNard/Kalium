// Keep a console attached even for release builds: Kalium includes a headless CLI.
// A GUI-only Windows launcher can be added as a separate binary later.

mod activity_log;
mod app;
mod bake;
mod blend;
mod brush_preset;
mod cli;
mod document;
mod export;
#[cfg(feature = "gpu-experimental")]
mod gpu_brush;
mod gpu_viewport;
mod history;
mod journal_writer;
#[allow(dead_code)] // S4 tree primitives await integration with Document and the compositor.
mod layer_tree;
mod layout;
mod mesh;
mod ops;
mod paint;
mod painter_classic;
#[allow(dead_code)] // S9 core is awaiting layer, persistence, history and UI integration.
mod procedural;
mod project;
mod recent_projects;
#[allow(dead_code)]
mod recovery;
mod recovery_store;
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
            .with_title("Kalium")
            .with_inner_size([1920.0, 1080.0])
            .with_maximized(true)
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

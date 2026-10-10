# Kalium

**Artist-first 3D texture painting. Open source. Written in Rust.**

Kalium is an independent texture-painting application aiming for interoperability with Adobe Substance 3D Painter and its asset ecosystem. Kalium is **not** an Adobe product and is not affiliated with Adobe.

> **CURRENT STATUS: v0.2.0-alpha.1 / DRAPNARD SOURCE RELEASE.** This repository is a *source prototype*, **NOT** a complete Substance Painter replacement or a feature-complete 1.0. In particular, `.spp` import/export, Substance brush presets and SBSAR execution are **not yet implemented**. This package was assembled in an environment without Rust installed, so it has **not been compiled or runtime-tested in this environment**. Tests are included to run on your own machine.

## Current features

- Native Rust desktop application using **egui/eframe + wgpu** for rendering the UI.
- **DrapNard** release identity; native format and project owner information documented in [AUTHORS.md](AUTHORS.md).
- 3D textured-mesh viewport, orbit camera (right-drag), zoom (scroll), triangle-based UV hit-testing, painting on the actual mesh surface.
- 2D UV painting workspace, optional UV wireframe overlay.
- Model imports: Wavefront **OBJ**, **glTF**, **GLB** (triangulated primitives with first UV set; transforms baked into the in-memory mesh).
- Eight PBR paint channels: **Base Color, Roughness, Metallic, Normal, Height, Emissive, Opacity, Ambient Occlusion**.
- Paint, erase, and paint white/black on layer masks; opacity, brush radius, hardness, spacing, U-axis mirroring, connected brush strokes.
- Editable layer stack with reordering, opacity, visibility, simple Normal/Multiply/Add blending and per-layer masks.
- Sparse, bounded Undo/Redo for brush strokes and full-channel fills where within the 96 MiB history budget; keyboard shortcuts.
- Fill/clear current channel or layer mask using the active tool mode, plus an eyedropper that samples the active channel in either 2D or 3D.
- Import PNG/JPEG textures into a new editable layer on the selected PBR channel.
- Native `.kbrush` JSON brush presets, with supplied Soft Painter and Hard Detail examples.
- **Headless CLI** to inspect/validate/export/editable-project-preview without launching the graphical UI.
- Native, documented, layered **`.kalium`** format, ZIP/JSON/PNG including embedded mesh and channels.
- Export 8 individual PBR channel PNG maps, BaseColor+Alpha, packed ORM (Occlusion/Roughness/Metallic), and Unity metallic/smoothness texture.
- Configurable texture size 256, 512, 1024, 2048, or 4096 (nearest-neighbor resampling).
- `.spp` **signature check only** (HDF5 identification, when applicable). This is **not `.spp` import**.
- No authentication, cloud services, AI generation, telemetry, or subscriptions.

### Rendering limitation (important)

The GUI uses wgpu, but the **3D mesh rasterizer is currently CPU-based**, with a simple preview shader and approximate PBR lighting, not full production PBR nor GPU compute painting. The CPU renderer generates a triangle UV hit-buffer and textured preview that is uploaded as a GPU image. Full `wgpu` native model rendering, compute-stroke projection, baking and UDIM streaming are future work.

### Start Kalium

You need a Rust toolchain (stable, sufficiently recent for egui 0.31 and image 0.25) and a working Vulkan/Metal/DX12 adapter. For **Arch Linux**:

```bash
sudo pacman -S --needed rustup base-devel pkgconf libxkbcommon wayland libx11 libxcursor libxrandr libxi vulkan-icd-loader
rustup default stable
cargo run --release
```

For the first build, Cargo must be able to download dependencies. If a dependency API changed in a newer crate release, use the bundled Cargo.toml as the basis for adapting the integration.

Tests and lint:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

This source release has *not* completed those checks in the generation environment. Run `cargo build` before relying on it for production work.

### Headless CLI

```bash
cargo run -- --version
cargo run -- inspect examples/Kalium-Demo.kalium
cargo run -- validate examples/Kalium-Demo.kalium
cargo run -- export examples/Kalium-Demo.kalium ./exported
cargo run -- preview examples/Kalium-Demo.kalium ./preview.png
cargo run -- brush-inspect examples/Soft-Painter.kbrush
cargo run -- spp-inspect path/to/owned-project.spp
```

SPP inspection checks only for an HDF5 signature. **It cannot read or save Substance Painter layers.** CLI commands share the same Rust library code as the GUI (module-level, not yet a published stable SDK).

### Controls

| Action | Input |
|---|---|
| Paint in 3D or 2D | Left mouse drag |
| Rotate model | Right mouse drag in 3D canvas |
| Zoom 3D | Mouse scroll |
| Toggle channels | PBR Channel selector |
| Edit mask | Tool → Mask + / Mask − |
| Save editable project | Ctrl+S |
| Open project | Ctrl+O |
| Undo / redo paint stroke | Ctrl+Z / Ctrl+Y |
| Import OBJ / GLTF / GLB | File → Import |
| Import editable PNG/JPEG layer | File → Import PNG/JPEG as layer |
| Fill current channel / mask | Brush Studio → Fill selected channel / mask |
| Load/save `.kbrush` preset | File → Load/Save brush preset |
| Export textures | File → Export PBR textures |

For a keyboard layout where Command is used, shortcuts adapt to the OS.

### Native project format

A `.kalium` file is an ordinary ZIP archive:

```text
MyTexture.kalium
├── manifest.json             # Version, mesh, layer stack, settings
└── layers/
    └── <numeric layer id>/
        ├── mask.png           # Grayscale layer mask
        ├── BaseColor.png
        ├── Roughness.png
        └── ...                # Sparse per-channel layers
```

It's intentionally independent of proprietary formats, editable, and inspectable without Kalium. See [docs/FORMAT.md](docs/FORMAT.md) and [docs/BRUSH_PRESETS.md](docs/BRUSH_PRESETS.md).

### What v0.2 does **not** yet provide

- Native GPU mesh rasterization, GPU brush projection/compute and physically accurate PBR.
- `.spp`, `.sppr`, `.sbsar` editing or any Adobe format round-trip. The formats remain a clean-room interoperability research goal.
- Baking, UDIM, material graphs, multi-texture-set workflows or professional-grade tablet pressure/tilt.
- A completed local Rust build/test run. This sandbox cannot fetch Rust or compile dependencies. Please run `cargo test` and `cargo build` before using this on real work.

### Roadmap toward a complete Painter replacement

See [docs/ROADMAP.md](docs/ROADMAP.md) and [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md). The targets include native GPU painting, real PBR rendering, UV and UDIM tooling, multi-material Texture Sets, mesh baking, procedural graph materials, smart materials and masks, asset browsing, brush engines and Substance format interoperability.

## License

Copyright © 2026 **DrapNard** and contributors. Source code is under **GNU Affero General Public License, version 3 only (AGPL-3.0-only)**. See [LICENSE](LICENSE). Third-party dependencies retain their own licenses. No Adobe branding, shader, resource or proprietary code is included.

Official direction: **tools for human artists, no generated imagery/materials in the official product**. See [PHILOSOPHY.md](PHILOSOPHY.md). This contributor policy applies to the project, not to what third parties can legally build under AGPLv3.

### Included demonstration file

Open `examples/Kalium-Demo.kalium` through **File → Open** to see a fully editable cube with hand-authored procedural swatches, layer masks and multiple channels. The sample is built from the documented Kalium format, contains no Adobe resources and uses no generative AI.

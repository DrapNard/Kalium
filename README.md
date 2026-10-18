# Kalium

**Artist-first 3D texture painting. Open source. Written in Rust.**

Kalium is an independent texture-painting application aiming for interoperability with Adobe Substance 3D Painter and its asset ecosystem. Kalium is **not** an Adobe product and is not affiliated with Adobe.

> **CURRENT STATUS: v0.3.0-alpha.2 / DRAPNARD SOURCE ALPHA.** This repository is a *source prototype*, **NOT** a complete Substance Painter replacement or a feature-complete 1.0. In particular, `.spp` import/export, Substance brush presets and SBSAR execution are **not yet implemented**. On 2026-10-18, the current worktree passes formatting, strict Clippy, all-target compilation, optional GPU feature compilation, and `cargo test --locked --all-targets` (**193/193 tests**). Headless egui interaction tests cover layer drag reorder, while desktop GUI/GPU behavior has not been visually validated. Recovery journals cover committed brush, fill, clear, mask, supported layer property, reorder, and selection edits and offer Restore/Discard on reopen; torn final frames recover the durable prefix and complete checksum-corrupt frames are rejected, while oversized patches and unsupported edits such as layer creation/removal disable recovery until the next save. This source-alpha build is not a production release.

## Material slots and multiple Texture Sets (alpha.2)

Kalium now preserves per-triangle material IDs from OBJ (`.mtl`) and glTF/GLB when importing a mesh. The **Texture Set List** pane includes **Mesh material bindings** where you can assign an editable set to each source material. An unbound material follows the active set. Click a surface with a bound material while painting to select its set automatically (toggle in the same pane).

Try it without downloading any model: **File → Import OBJ** and select `examples/TwoMaterials.obj` (keep `TwoMaterials.mtl` alongside it). Kalium initially binds all new mesh materials to the active set. Add a second Texture Set and bind **Metal** to the new set; **Leather** stays bound to the original set, even when you switch active sets. The two halves now display independently editable materials. The UV overlay/clipping follows the active set, and project save writes `.kalium` manifest **v6**, which includes a stable project UUID as groundwork for future identity-bound recovery. Versions 1–5 remain readable and receive a new identity when migrated; older Kalium builds will not open v6 projects. Make a backup before resaving.

The 3D viewport still uses a **CPU software rasterizer**, not a new GPU PBR pipeline. `.spp` editing and Smart Materials remain unsupported; baking is limited to standalone CLI commands and is not saved into projects or shown in the GUI.

## Current features

- Native Rust desktop application using **egui/eframe + wgpu** for rendering the UI.
- **Dockable professional workspace**: draggable tabbed Assets, 3D Viewport, 2D/UV editor, Texture Set List, Layers, Texture Set Settings, Properties and Brush Studio. Panels can be resized, undocked into native floating windows, closed and restored via View.
- **Side-by-side 3D and UV viewports**, inspired by the supplied layout reference (without copying branding or bundling the reference image). The CPU 3D framebuffer automatically resizes with its dock panel while preserving the orbit camera and UV picking buffer.
- Narrow, optional vertical tool shelf, channel/layer toolbar, B/E/I/G tool shortcuts (ignored when typing), and a non-destructive brush-cursor preview.
- Add, duplicate, remove, and reorder layers; rename layers and edit visibility, opacity, blend mode, and channel enablement with Undo/Redo. Detail Normal layers use reoriented normal mapping; texture imports are undoable and preserve prior history. Removed layers that exceed the 96 MiB history budget are rejected before deletion.
- Save and recover a manual per-user project checkpoint from the File menu; checkpoint recovery is explicit and requires Save As. Timed autosave is not implemented yet.
- Per-user workspace layout persistence in the platform config directory, independently from project files; View → Restore default workspace.
- **DrapNard** release identity; native format and project owner information documented in [AUTHORS.md](AUTHORS.md).
- 3D textured-mesh viewport, orbit camera (right-drag), zoom (scroll), triangle-based UV hit-testing, painting on the actual mesh surface.
- 2D UV painting workspace, optional UV wireframe overlay. UV-polygon clipping driven by a bounded triangle rasterizer; empty atlas areas are protected by default.
- Model imports: Wavefront **OBJ**, **glTF**, **GLB** (triangulated primitives with first UV set; transforms baked into the in-memory mesh).
- Eight PBR paint channels: **Base Color, Roughness, Metallic, Normal, Height, Emissive, Opacity, Ambient Occlusion**.
- Normal-channel strokes blend and renormalize signed vectors. The optional **Detail Normal** layer mode combines tangent-space detail with accumulated normals.
- Paint, erase, and paint white/black on layer masks; opacity, brush radius, hardness, spacing, U-axis mirroring, connected brush strokes.
- 4-connected flood-fill tool, configurable tolerance, optional UV-polygon clipping and sparse undo history.
- Editable layer stack with reordering, opacity, visibility, simple Normal/Multiply/Add blending and per-layer masks.
- Sparse, bounded Undo/Redo for brush strokes, full-channel fills, layer-stack commands and layer properties, within a 96 MiB history budget; keyboard shortcuts.
- Fill/clear current channel or layer mask using the active tool mode, plus an eyedropper that samples the active channel in either 2D or 3D.
- Import PNG/JPEG textures into a new editable layer on the selected PBR channel.
- Native `.kbrush` JSON brush presets, with supplied Soft Painter and Hard Detail examples.
- **Headless CLI** to inspect/validate/export/editable-project-preview and export UV coverage masks without launching the graphical UI.
- Experimental headless mesh-map bake for same-mesh world normals and normalized positions, with coverage and encoding metadata. It does not provide an artist-facing high-to-low workflow or persist maps in the project.
- **Experimental opt-in GPU compute stamp** (WGSL / wgpu; CLI only via `--features gpu-experimental`; not yet integrated into the realtime editor and not validated on hardware).
- **Multiple editable Texture Sets** with independent resolutions, layers, masks, PBR channel switches and stable set IDs. Imported mesh material slots can be explicitly bound to Texture Sets in the Texture Set List pane; the CPU viewport previews them simultaneously.
- Native, documented, layered **`.kalium`** v6 format, ZIP/JSON/PNG including embedded mesh, material-to-set bindings and all Texture Sets; backward-compatible loading of v1–v5.
- Export 8 individual PBR channel PNG maps, BaseColor+Alpha, packed ORM (Occlusion/Roughness/Metallic), and Unity metallic/smoothness texture, for either the active set or every set into individual named folders. Export settings choose OpenGL (+Y) or DirectX (−Y) Normal maps and optional UV edge padding.
- Configurable texture size 256, 512, 1024, 2048, or 4096 (nearest-neighbor resampling).
- `.spp` **signature check only** (HDF5 identification, when applicable). This is **not `.spp` import**.
- No authentication, cloud services, AI generation, telemetry, or subscriptions.

### Texture Sets (v0.3.0-alpha.2)

**Texture Set List** now has an **Add set** button, a per-set selector, a rename field and a guarded remove button. Switching sets is a zero-copy transfer of texture buffers, and clears brush Undo/Redo rather than allowing history collisions with identically numbered layers in other sets. **Texture Set Settings** edits the active set's resolution independently. `File → Export active set PBR textures` preserves the old export workflow; `File → Export all texture sets` creates numbered, sanitized subfolders and offers optional UV-aware edge padding.

`.kalium` manifest **v6** saves project identity and material bindings and the inactive sets under `texture_sets/<set id>/layers/<layer id>/...` and keeps the active set's image files under the legacy `layers/...` path, while v1–v5 documents continue to open. v5 adds the Detail Normal layer blend mode; v6 adds a stable project UUID reserved for associating journal recovery with a saved baseline. Old Kalium versions cannot read v6. Limits: 16 sets, 64 layers per set, 512 MiB decoded texture budget. **Back up your project before migrating it**.

**Limitations:** material names and slots are imported but Substance procedural graphs, shading models and source texture files are not. The multi-material viewport remains a CPU reference rasterizer, not a native PBR GPU pipeline. Do not treat this as full Substance Painter parity.

### Workspace serialization and channel switches (v0.2.0-alpha.5)

The `egui_dock` layout JSON round-trip failure (`invalid type: null, expected f32`) is addressed by normalizing uninitialized floating-point rectangle coordinates during workspace serialization and when loading old layout files. Automated tests cover layout round-tripping and repair of old files with null geometry.

The **Layers** panel has individual channel enable switches. Disabled channels are composited/exported as if that layer had no influence on the channel, while painted pixel data is retained. Channel switches and other layer properties participate in bounded Undo/Redo alongside brush strokes, fills, layer add/duplicate/remove/reorder, and masks. The current `.kalium` format is manifest **v6**; it continues to load v1–v5 documents.

### Dockable UI workspace (v0.2.0-alpha.5)

Use **View → Show / hide panels** to reopen closed panes, **View → Restore default workspace** to reset the desktop-like arrangement, or drag a tab header to dock/undock/reorder panels. All panel sizes, tabs and floating positions are serialized to a user configuration file. This is editor-level state and is **not** saved inside `.kalium` projects.

- Linux: `${XDG_CONFIG_HOME:-$HOME/.config}/kalium/workspace-v1.json`
- Windows: `%APPDATA%\kalium\workspace-v1.json`
- macOS (when `XDG_CONFIG_HOME` is not configured): `~/.config/kalium/workspace-v1.json`. Native Application Support is a future platform improvement.

The v0.2.0-alpha.4 default layout gives additional width to the 3D and 2D workspaces. Existing layout files will **not** be overwritten by this update; use **View → Restore default workspace** to switch to the new proportions.

The Assets panel exposes the implemented brush presets and texture-import operations. It does not ship the materials shown in the design reference. Texture Set List supports independent editable sets and mesh-material bindings; Substance preset parsing and Smart Materials are not implemented.

### Experimental compute brush

```bash
# Build and check optional GPU module (requires a working Rust toolchain / graphics drivers)
cargo check --features gpu-experimental
cargo run --features gpu-experimental -- gpu-stamp in.png out.png 0.5 0.5 30

# Export a reference mesh UV coverage mask (white=paintable)
cargo run -- uv-mask examples/Kalium-Demo.kalium coverage.png
cargo run -- bake-mesh-maps examples/Kalium-Demo.kalium ./baked-maps [texture-set-id]
cargo run -- bake-ao examples/Kalium-Demo.kalium ao.png [texture-set-id] [samples] [max-distance] [seed]
```

See [docs/GPU_COMPUTE.md](docs/GPU_COMPUTE.md). The experimental module creates its own GPU device and copies buffers back to the CPU, so it is not yet suitable for interactive real-time painting. `bake-ao` writes a deterministic grayscale map (white=open, black=occluded), defaulting to 16 samples, max distance 1.0, and seed 0.

### Rendering limitation (important)

The GUI uses wgpu, but the **3D mesh rasterizer is currently CPU-based**. Its reference shader uses linear-light Cook–Torrance GGX direct lighting, normal maps, emissive and an environment-free ambient fill; it has no image-based lighting, configurable studio environment, controlled visual acceptance or GPU mesh rendering. The CPU renderer generates a triangle UV hit-buffer and textured preview that is uploaded as a GPU image. The experimental mesh-map command bakes same-mesh world normals and normalized positions; the separate `bake-ao` command writes bounded same-mesh ambient occlusion. An internal tested high-to-low projection primitive is not exposed as an artist workflow until shared mesh alignment and cage controls are persisted. Curvature, dilation, project persistence and GUI baking remain future work. Native `wgpu` model rendering, compute-stroke projection and UDIM streaming are future work.

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

Formatting, strict Clippy, all-target compilation, optional GPU feature compilation, and unit tests passed on the current worktree on 2026-10-18. GUI startup, graphics hardware behavior, cross-platform builds, and downstream-engine imports have not been verified. Perform the documented platform/visual checks before relying on it for production work.

### Headless CLI

```bash
cargo run -- --version
cargo run -- inspect examples/Kalium-Demo.kalium
cargo run -- validate examples/Kalium-Demo.kalium
cargo run -- export examples/Kalium-Demo.kalium ./exported
cargo run -- export-all examples/Kalium-Demo.kalium ./exported-by-set
cargo run -- preview examples/Kalium-Demo.kalium ./preview.png
cargo run -- bake-ao examples/Kalium-Demo.kalium ./ambient-occlusion.png
cargo run -- brush-inspect examples/Soft-Painter.kbrush
cargo run -- spp-inspect path/to/owned-project.spp
```

SPP inspection checks only for an HDF5 signature. **It cannot read or save Substance Painter layers.** CLI commands share the same Rust library code as the GUI (module-level, not yet a published stable SDK).

### Controls

| Action | Input |
|---|---|
| Paint in 3D or 2D | Left mouse drag |
| Brush / Eraser / Eyedropper / Flood Fill | B / E / I / G |
| Duplicate active layer | Layer → Duplicate selected layer |
| Invert or reset selected layer mask | Layer menu or Layers pane |
| Clear active channel (Undo available within budget) | Layer → Clear active channel |
| Rotate model | Right mouse drag in 3D canvas |
| Zoom 3D | Mouse scroll |
| Toggle channels | PBR Channel selector |
| Edit mask | Tool → Mask + / Mask − |
| Save editable project | Ctrl+S |
| Open project | Ctrl+O |
| Undo / redo paint or layer edit | Ctrl+Z / Ctrl+Y |
| Import OBJ / GLTF / GLB | File → Import |
| Import editable PNG/JPEG layer | File → Import PNG/JPEG as layer |
| Fill current channel / mask | Brush Studio → Fill selected channel / mask |
| Load/save `.kbrush` preset | File → Load/Save brush preset |
| Export active textures | File → Export active set PBR textures |
| Export every Texture Set | File → Export all texture sets |
| Switch / create / rename a set | Texture Set List panel |

For a keyboard layout where Command is used, shortcuts adapt to the OS.

### Native project format

A `.kalium` file is an ordinary ZIP archive:

```text
MyTexture.kalium
├── manifest.json             # Version, mesh, Texture Set metadata
├── layers/
    └── <numeric layer id>/
        ├── mask.png           # Grayscale layer mask
        ├── BaseColor.png
        ├── Roughness.png
        └── ...                # Sparse per-channel layers
└── texture_sets/             # Additional (inactive) sets in v3
    └── <texture set id>/layers/<numeric layer id>/...
```

It's intentionally independent of proprietary formats, editable, and inspectable without Kalium. See [docs/FORMAT.md](docs/FORMAT.md) and [docs/BRUSH_PRESETS.md](docs/BRUSH_PRESETS.md).

### Current limitations

- Native GPU mesh rasterization, GPU brush projection/compute or production PBR rendering. The CPU rasterizer remains the reference implementation.
- `.spp`, `.sppr`, `.sbsar` editing or any Adobe format round-trip. The formats remain a clean-room interoperability research goal.
- Baking, UDIM, material graphs, layer groups/fill layers, or professional-grade tablet pressure/tilt.
- Base Color and Emissive brush stamps and layer composites use linear light. Normal strokes and ordinary layers operate on normalized vectors; Detail Normal layers use RNM. Scalar channels blend data values directly. The separate `Kalium_BaseColor_Alpha.png` takes alpha from the Opacity channel's red value; downstream engine expectations remain open.
- GUI-driven visual acceptance, downstream-engine imports, cross-platform builds and release packages. The current Linux source tree passes its documented local checks; see [the QA matrix](docs/qa/test-matrix.md).

### Roadmap toward a complete Painter replacement

See [docs/ROADMAP.md](docs/ROADMAP.md) and [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md). The remaining targets include native GPU painting, production PBR rendering, UV and UDIM tooling, mesh baking, procedural graph materials, smart materials and masks, asset browsing, and further brush workflows.

## License

Copyright © 2026 **DrapNard** and contributors. Source code is under **GNU Affero General Public License, version 3 only (AGPL-3.0-only)**. See [LICENSE](LICENSE). Third-party dependencies retain their own licenses. No Adobe branding, shader, resource or proprietary code is included.

Official direction: **tools for human artists, no generated imagery/materials in the official product**. See [PHILOSOPHY.md](PHILOSOPHY.md). This contributor policy applies to the project, not to what third parties can legally build under AGPLv3.

### Included demonstration file

Open `examples/Kalium-Demo.kalium` through **File → Open** to see a fully editable cube with hand-authored procedural swatches, layer masks and multiple channels. The sample is built from the documented Kalium format, contains no Adobe resources and uses no generative AI.

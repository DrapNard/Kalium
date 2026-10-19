# Kalium

**Artist-first 3D texture painting. Open source. Written in Rust.**

[Public source repository](https://github.com/DrapNard/Kalium)

Kalium is an independent texture-painting application aiming for interoperability with Adobe Substance 3D Painter and its asset ecosystem. Kalium is **not** an Adobe product and is not affiliated with Adobe.

> **CURRENT STATUS: v0.3.0-alpha.2 / DRAPNARD SOURCE ALPHA.** This repository is a *source prototype*, **NOT** a complete Substance Painter replacement or a feature-complete 1.0. In particular, `.spp` import/export, Substance brush presets and SBSAR execution are **not yet implemented**. The current worktree is being revalidated after the S4 layer-tree integration; see `docs/qa/test-matrix.md` and `docs/roadmap/implementation-status.md` for the latest gate results. The CPU reference viewport now has bounded directional-key and ambient-fill controls plus deterministic Studio, Warm and Cool analytic environments; HDR image-based lighting and desktop visual acceptance remain unverified. Recursive Paint/Fill/Group trees are wired to Documents and Texture Sets, persisted in manifest v9, and covered by nested composition, mutation-bound, migration, stable-ID history/recovery, and headless Add Group/Fill→edit→save/reopen→Restore tests. Manifest preflight budgets retained extension metadata across project, Texture Set and tree scopes. Other coverage includes a bounded per-user Activity pane, layer selection save/recovery, pre-allocation ZIP/ZIP64 directory bounds, glTF UV0–7 preservation plus fail-closed deformation imports, workspace close-time flushing/atomic replacement, resize transient-memory bounds, OBJ short-face triangulation caps, model strokes across duplicated UV seams, and pointer-anchored UV zoom/pan painting. An experimental WGPU model preview now draws depth-tested mesh triangles and samples the bound Base Color Texture Set; CPU picking and painting remain authoritative, and a basic preview has been observed under Xvfb. Full rendering correctness and desktop visual/upload acceptance remain unverified. The CPU renderer remains the reference implementation; full GPU PBR, GPU picking and GPU painting are future work. This source-alpha build is not a production release.

## Current development priority

Real editable `.spp` open/save is the owner’s priority and target project workflow.
It is **not implemented yet**; current native saves use `.kalium`. See
[SPP acceptance plan](docs/roadmap/spp-focus.md). Native Save refuses `.spp`
filenames to avoid overwriting a Painter project with a mislabeled archive.

## Material slots and multiple Texture Sets (alpha.2)

Kalium now preserves per-triangle material IDs from OBJ (`.mtl`) and glTF/GLB when importing a mesh. The **Texture Set List** pane includes **Mesh material bindings** where you can assign an editable set to each source material. An unbound material follows the active set. Click a surface with a bound material while painting to select its set automatically (toggle in the same pane).

Try it without downloading any model: **File → Import OBJ** and select `examples/TwoMaterials.obj` (keep `TwoMaterials.mtl` alongside it). Kalium initially binds all new mesh materials to the active set. Add a second Texture Set and bind **Metal** to the new set; **Leather** stays bound to the original set, even when you switch active sets. The two halves now display independently editable materials. The UV overlay/clipping follows the active set, and project save writes `.kalium` manifest **v8**, which includes a stable project UUID and optional procedural layer graphs. Versions 1–7 remain readable; older Kalium builds will not open v8 projects. Make a backup before resaving.

The 3D viewport still uses a **CPU software rasterizer**, not a new GPU PBR pipeline. `.spp` editing and Smart Materials remain unsupported. Ambient occlusion can be baked in the GUI on a cancellable worker and installed as an editable Occlusion layer. Per-Texture-Set sample count, ray distance and seed are saved with the project and journaled for recovery.

## Current features

- Native Rust desktop application using **egui/eframe + wgpu** for rendering the UI.
- **Painter-style dockable workspace**: dedicated Assets column, side-by-side 3D and UV canvases, and a right stack for Texture Set List, Layers, and Properties, with Brush Studio and settings in contextual tabs. The menu and compact top toolbar expose File/Edit/Mode/Window/Viewport, brush size/flow/hardness/spacing, and channel selection. Panels can be resized, undocked into native floating windows, closed and restored via Window.
- **Side-by-side 3D and UV viewports** in the Painter-style dock layout. The CPU 3D framebuffer automatically resizes with its dock panel while preserving the orbit camera and UV picking buffer.
- Narrow, optional vertical tool shelf, channel/layer toolbar, B/E/I/G tool shortcuts (ignored when typing), and a non-destructive brush-cursor preview.
- Add, duplicate, remove, and reorder layers; rename layers and edit visibility, opacity, blend mode, and channel enablement with Undo/Redo. Solo the selected Paint, Fill, or Group as a transient preview while preserving its ancestor Group compositing. Detail Normal layers use reoriented normal mapping; texture imports are undoable and preserve prior history. Removed layers that exceed the 96 MiB history budget are rejected before deletion.
- Save and recover a manual per-user project checkpoint from the File menu; checkpoint recovery is explicit and requires Save As. Untitled-session edits use a per-user baseline plus recovery journal with Restore/Discard and an OS advisory lock. At 16 MiB, recovery publishes a generation-bound checkpoint and matching empty journal through an atomic manifest switch; the writer drain and compaction run on a worker while the UI shows progress; editing and file actions pause until the generation switch completes.
- Per-user workspace layout persistence in the platform config directory, independently from project files; Window → Restore default workspace.
- **DrapNard** release identity; native format and project owner information documented in [AUTHORS.md](AUTHORS.md).
- 3D textured-mesh viewport, orbit camera (right-drag), zoom (scroll), triangle-based UV hit-testing, painting on the actual mesh surface.
- 2D UV painting workspace, optional UV wireframe overlay. UV-polygon clipping driven by a bounded triangle rasterizer; empty atlas areas are protected by default.
- Model imports: Wavefront **OBJ**, **glTF**, **GLB** (triangulated primitives with complete UV0 required; missing UVs return an actionable error instead of collapsing paint to one texel; glTF TEXCOORD_1–7 are retained in project mesh data while painting remains on UV0; skinning/morph data fail closed; transforms are baked into the in-memory mesh). Optional glTF tangents and handedness are preserved through node transforms and used by the CPU reference viewport's normal-map shading; absent tangents retain the UV-derived frame.
- Eight PBR paint channels: **Base Color, Roughness, Metallic, Normal, Height, Emissive, Opacity, Ambient Occlusion**.
- Normal-channel strokes blend and renormalize signed vectors. The optional **Detail Normal** layer mode combines tangent-space detail with accumulated normals.
- Paint, erase, and paint white/black on layer masks; opacity, brush radius, hardness, spacing, U-axis mirroring, connected brush strokes.
- 4-connected flood-fill tool, configurable tolerance, optional UV-polygon clipping and sparse undo history.
- Editable layer stack with reordering, opacity, visibility, simple Normal/Multiply/Add blending and per-layer masks.
- Sparse, bounded Undo/Redo for brush strokes, full-channel fills, layer-stack commands and layer properties, within a 96 MiB history budget; keyboard shortcuts.
- Fill/clear current channel or layer mask using the active tool mode, plus an eyedropper that samples the active channel in either 2D or 3D.
- Import PNG/JPEG textures into a new editable layer on the selected PBR channel.
- Native `.kbrush` JSON brush presets (v1 compatible), with supplied Soft Painter and Hard Detail examples; bounded PNG/JPEG alpha tips apply to paint and layer-mask strokes.
- **Headless CLI** to inspect/validate/export/editable-project-preview and export UV coverage masks without launching the graphical UI.
- Headless mesh-map bake for same-mesh world normals and normalized positions, and high-to-low world-normal projection with an optional affine high-mesh transform. Both write standalone maps and metadata; high-to-low has no cage workflow, dilation or project-persisted output.
- Export a standalone material-ID PNG from File or Texture Set Settings; a cancellable CPU worker writes a matching slot-encoding JSON sidecar without editing the project.
- Bake ambient occlusion on a background CPU worker and install the result into an editable, undoable Occlusion layer. Texture Set Settings exposes sample count and ray distance; the UI shows row progress and supports cancellation. Defaults are 16 samples, ray distance 1.0 and seed 0.
- **Experimental opt-in GPU compute stamp** (WGSL / wgpu; CLI only via `--features gpu-experimental`; not yet integrated into the realtime editor and not validated on hardware).
- **Multiple editable Texture Sets** with independent resolutions, layers, masks, PBR channel switches and stable set IDs. Imported mesh material slots can be explicitly bound to Texture Sets in the Texture Set List pane; the CPU viewport previews them simultaneously.
- Native, documented, layered **`.kalium`** v9 format, ZIP/JSON/PNG including embedded mesh, material-to-set bindings, all Texture Sets, recursive Paint/Fill/Group trees and optional procedural Roughness graphs; backward-compatible loading of v1–v8.
- Export 8 individual PBR channel PNG maps, Base Color+Alpha, packed ORM (Occlusion/Roughness/Metallic), and Unity metallic/smoothness texture, for the active set or all sets. Named Godot 4, Unity metallic, and glTF 2.0 metallic-roughness profiles write per-set channel manifests and pin OpenGL (+Y) Normal maps; custom export still offers OpenGL (+Y) or DirectX (−Y) and optional UV edge padding. These sidecars describe manual material setup; they do not create engine materials or glTF model files, and editor import/render behavior remains unverified.
- Configurable texture size 256, 512, 1024, 2048, or 4096 (nearest-neighbor resampling).
- `.spp` **signature check only** (HDF5 identification, when applicable). This is **not `.spp` import**.
- No authentication, cloud services, AI generation, telemetry, or subscriptions.

### Texture Sets (v0.3.0-alpha.2)

**Texture Set List** now has an **Add set** button, a per-set selector, a rename field and a guarded remove button. Switching sets is a zero-copy transfer of texture buffers, and clears brush Undo/Redo rather than allowing history collisions with identically numbered layers in other sets. **Texture Set Settings** edits the active set's resolution independently. `File → Export active set PBR textures` preserves the old export workflow; `File → Export all texture sets` creates numbered, sanitized subfolders and offers optional UV-aware edge padding.

`.kalium` manifest **v9** stores recursive Paint/Fill/Group trees, project identity, material bindings, procedural graphs and inactive Texture Sets. Versions 1–8 continue to open; their flat stacks migrate to Paint roots. v8 introduced AO graph inputs. Older Kalium versions cannot read v9. Limits include 16 sets, 64 Paint layers per set and a 512 MiB decoded texture budget.

**Limitations:** material names and slots are imported but Substance procedural graphs, shading models and source texture files are not. The multi-material viewport remains a CPU reference rasterizer, not a native PBR GPU pipeline. Do not treat this as full Substance Painter parity.

File → Open Recent lists up to 12 successfully opened or saved projects. It uses the same unsaved-change and recovery guards as Open. Missing files are disabled and removable; Clear recent projects clears only this user preference. Paths are stored in `recent-projects-v1.json` beside the per-user workspace file, outside `.kalium` projects.

### Workspace serialization and channel switches (v0.2.0-alpha.5)

The `egui_dock` layout JSON round-trip failure (`invalid type: null, expected f32`) is addressed by normalizing uninitialized floating-point rectangle coordinates during workspace serialization and when loading old layout files. Automated tests cover layout round-tripping and repair of old files with null geometry.

The **Layers** panel has individual channel enable switches, recursive Groups, Fill values and optional seeded procedural Roughness graphs with editable seed and scale. An AO-driven graph can use the raw Occlusion channel from another same-set layer; missing source layers fall back to seeded noise. Disabled channels are composited/exported as if that layer had no influence on the channel, while painted pixel data is retained. Channel switches and other layer properties participate in bounded Undo/Redo and crash recovery alongside brush strokes, fills, layer add/duplicate/remove/reorder, and masks. The current `.kalium` format is manifest **v9**; it continues to load v1–v8 documents.

### Dockable UI workspace (v0.2.0-alpha.5)

Use **Window → Show / hide panels** to reopen closed panes, **Window → Restore default workspace** to reset the desktop-like arrangement, or drag a tab header to dock/undock/reorder panels. All panel sizes, tabs and floating positions are serialized to a user configuration file. This is editor-level state and is **not** saved inside `.kalium` projects.

- Linux: `${XDG_CONFIG_HOME:-$HOME/.config}/kalium/workspace-v1.json`
- Windows: `%APPDATA%\kalium\workspace-v1.json`
- macOS (when `XDG_CONFIG_HOME` is not configured): `~/.config/kalium/workspace-v1.json`. Native Application Support is a future platform improvement.

The default workspace now follows the Substance Painter panel structure with an Assets column, side-by-side 3D/UV canvases and a stacked right-side layer workflow. Existing per-user layout files are preserved; use **Window → Restore default workspace** to switch to the current defaults.

The Assets panel exposes the implemented brush presets and texture-import operations. It does not ship the materials shown in the design reference. Texture Set List supports independent editable sets and mesh-material bindings; Substance preset parsing and Smart Materials are not implemented.

### Experimental compute brush

```bash
# Build and check optional GPU module (requires a working Rust toolchain / graphics drivers)
cargo check --features gpu-experimental
cargo run --features gpu-experimental -- gpu-stamp in.png out.png 0.5 0.5 30

# Export a reference mesh UV coverage mask (white=paintable)
cargo run -- uv-mask examples/Kalium-Demo.kalium coverage.png
cargo run -- bake-mesh-maps examples/Kalium-Demo.kalium ./baked-maps [texture-set-id]
cargo run -- bake-high-to-low examples/Kalium-Demo.kalium ./high.obj ./projected-maps [texture-set-id] [max-distance] [high-transform.json]
cargo run -- bake-material-id examples/Kalium-Demo.kalium ./material-id.png [texture-set-id]
cargo run -- bake-ao examples/Kalium-Demo.kalium ao.png [texture-set-id] [samples] [max-distance] [seed]
```

`bake-high-to-low` uses Kalium's viewport-normalized imported high mesh when no transform is supplied. With `high-transform.json`, it preserves source OBJ coordinates (or glTF coordinates after node transforms) and applies a 16-number, column-major affine matrix into the low project's stored coordinate space. `max-distance` is measured in low-project units after that transform. Transformed coordinates are capped at 1,000,000 units. The command writes world-normal, coverage and metadata files as standalone outputs; correspondence cages, dilation and project-persisted maps are not included. `bake-material-id` writes a standalone 24-bit little-endian material-slot ID PNG and a `.png.json` encoding/slot-name sidecar; it does not add a project asset or alter the PBR layers. `bake-ao` writes a deterministic grayscale map (white=open, black=occluded), defaulting to 16 samples, max distance 1.0, and seed 0. See [docs/GPU_COMPUTE.md](docs/GPU_COMPUTE.md). The experimental module creates its own GPU device and copies buffers back to the CPU, so it is not yet suitable for interactive real-time painting.

### Rendering limitation (important)

The CPU renderer remains the full-featured reference path for PBR shading, mesh picking and painting. The optional experimental WGPU preview draws depth-tested mesh geometry with bound Base Color textures and simple diffuse lighting; metallic/roughness/normal/emissive shading, GPU picking and GPU painting are not implemented. The GPU shader and CPU scene-routing contracts have hardware-free tests. A live app instance opened with the host WGPU/NVIDIA stack, but its preview output could not be captured here; rendered output and desktop visual acceptance remain unverified. The CPU reference shader uses linear-light Cook–Torrance GGX direct lighting, normal maps, emissive, bounded key-light controls, ambient fill and deterministic analytic Studio/Warm/Cool environments; it has no HDR image-based lighting. The CPU renderer also generates a triangle UV hit-buffer and textured preview that is uploaded as a GPU image. Headless mesh-map commands bake same-mesh maps and project high-mesh world normals onto a low-mesh Texture Set. High-to-low accepts an optional source-coordinate affine transform, but has no correspondence cage, dilation or project-persisted outputs. AO is available both as a GUI-generated editable Occlusion layer and through `bake-ao`, with per-Texture-Set settings persisted and journaled. Curvature, dilation and generated-map storage remain future work.

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

Formatting, strict Clippy, all-target compilation, optional GPU feature compilation, and unit tests passed on the current worktree on 2026-10-19. GUI startup, graphics hardware behavior, cross-platform builds, and downstream-engine imports have not been verified. Perform the documented platform/visual checks before relying on it for production work.

### Headless CLI

```bash
cargo run -- --version
cargo run -- inspect examples/Kalium-Demo.kalium
cargo run -- validate examples/Kalium-Demo.kalium
cargo run -- export examples/Kalium-Demo.kalium ./exported
cargo run -- export-all examples/Kalium-Demo.kalium ./exported-by-set
cargo run -- export-all-godot examples/Kalium-Demo.kalium ./godot-maps
cargo run -- export-all-unity examples/Kalium-Demo.kalium ./unity-maps
cargo run -- export-all-gltf examples/Kalium-Demo.kalium ./gltf-maps
cargo run -- export-all-painter examples/Kalium-Demo.kalium ./painter-maps
cargo run -- preview examples/Kalium-Demo.kalium ./preview.png
cargo run -- bake-ao examples/Kalium-Demo.kalium ./ambient-occlusion.png
cargo run -- brush-inspect examples/Soft-Painter.kbrush
cargo run -- spp-inspect path/to/owned-project.spp
```

SPP inspection checks only for an HDF5 signature. **It cannot read or save `.spp` projects.** The Painter export profile writes standard PNG maps plus a JSON `name`/`maps` preset payload for Painter's documented Python export API. It is not an `.spexp` shelf file. Painter can import the exported PNG maps; Kalium can import PNG layers. CLI commands share the same Rust library code as the GUI (module-level, not yet a published stable SDK).

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
- Full UDIM painting, broad material-graph editing, or professional-grade tablet pressure/tilt.
- Base Color and Emissive brush stamps and layer composites use linear light. Normal strokes and ordinary layers operate on normalized vectors; Detail Normal layers use RNM. Scalar channels blend data values directly. The separate `Kalium_BaseColor_Alpha.png` takes alpha from the Opacity channel's red value; downstream engine expectations remain open.
- GUI-driven visual acceptance, Godot/Unity/glTF downstream imports, cross-platform builds and release packages. Export profile maps and sidecars have automated assertions and CLI artifact checks, but runtime imports remain unverified. See [the QA matrix](docs/qa/test-matrix.md).

### Roadmap toward a complete Painter replacement

See [docs/ROADMAP.md](docs/ROADMAP.md) and [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md). The remaining targets include native GPU painting, production PBR rendering, UV and UDIM tooling, mesh baking, procedural graph materials, smart materials and masks, asset browsing, and further brush workflows.

## License

Copyright © 2026 **DrapNard** and contributors. Source code is under **GNU Affero General Public License, version 3 only (AGPL-3.0-only)**. See [LICENSE](LICENSE). Third-party dependencies retain their own licenses. No Adobe branding, shader, resource or proprietary code is included.

Official direction: **tools for human artists, no generated imagery/materials in the official product**. See [PHILOSOPHY.md](PHILOSOPHY.md). This contributor policy applies to the project, not to what third parties can legally build under AGPLv3.

### Included demonstration file

Open `examples/Kalium-Demo.kalium` through **File → Open** to see a fully editable cube with hand-authored procedural swatches, layer masks and multiple channels. The sample is built from the documented Kalium format, contains no Adobe resources and uses no generative AI.

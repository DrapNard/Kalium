# Kalium changelog

Kalium author and release identity: **DrapNard** (https://github.com/DrapNard). All changes distributed as AGPL-3.0-only.

## v0.2.0-alpha.4 — 2026-10-10

DrapNard source release. Substance Painter UI parity is the long-term design target, not an achieved feature claim. Changes are written in real Rust code; **compilation and GUI behavior must still be validated on a Rust-equipped machine**.

### Added and improved
- Narrower Assets strip, wider 3D/2D workspace and right-side Properties stack in the default dock layout; saved user layouts are kept unchanged until **View → Restore default workspace**.
- Vertical, hideable painting tool shelf with Brush, Eraser, Eyedropper, Flood Fill and black/white mask modes.
- Paint shortcuts B/E/I/G (disabled while a text control owns keyboard focus).
- Deep layer duplication with fresh stable IDs, editable copied masks and all PBR channels; add/delete/reorder commands accessible in Layer menu and Layers pane.
- Active-channel clear and layer-mask invert/reset using the existing sparse Undo/Redo snapshots; panel actions and menu commands operate on the actual editable document.
- Per-layer indicators for populated channels and modified masks; active channel selector beside layer controls.
- Responsive 3D framebuffer resizing when docking or adjusting the panel split, without discarding the orbit camera; pointer UV hit buffer follows the render dimensions.
- Non-destructive cursor-radius overlay on painting canvases; project exports are unchanged.
- Regression tests for duplicated layer independence, mask inversion and viewport resizing.

### Warning fixes
- Explicit `f32` sizes for the two reported `egui::Stroke::new` warnings.
- Explicitly discard the `Ui::selectable_label` response in the currently single-set Texture Set pane.
- Annotated the intentionally retained public `Brush::stamp`/`stroke` APIs to suppress dead-code warnings until plug-in consumers exist.

### Still missing
- More than one Texture Set, real-time GPU 3D rasterization and painting, UDIMs, advanced material/fill layers, baking, proprietary `.spp`/`.sppr`/`.spsm` round-trips and `.sbsar` execution. The docking layout does not imply these are implemented.

## v0.2.0-alpha.3 — 2026-10-10

UI/workspace iteration by **DrapNard**. Source-only update: build and interactive execution were **not verified**, since Rust/Cargo are absent from the available environment.

### Added
- Replaced fixed sidebars and single-view toggle with a real `egui_dock` workspace: eight dockable, reorderable, detachable, closable panes.
- 3D and UV/2D views are independent and visible simultaneously in the default layout, preserving painting/picking behavior in both.
- New panels and toolbar structure: Assets, Texture Set List, Layers, Texture Set Settings, Properties, Brush Studio and 3D/2D viewports.
- View menu provides show/hide per panel, restore default workspace and manual save; user-specific workspace state is automatically written via JSON and loaded on launch.
- Editor settings kept separate from `.kalium` project files; workspace layout tests and a dedicated layout architecture note.
- Tuned the dark desktop palette and blue selection highlights toward the supplied reference. No generated or modified image is used in the application.

### Deliberate limitations
- Source cannot be claimed compilable or visually pixel-perfect until tested using Rust/Cargo and real desktop graphics backends.
- Assets tab contains only implemented Kalium brush presets and imports; multi-texture sets and Adobe formats remain unimplemented.
- The GPU brush is still an opt-in CLI experiment; neither real-time GPU painting nor advanced PBR baking was introduced in this iteration.

## v0.2.0-alpha.2 — 2026-10-10

Third source iteration by **DrapNard**. Rust toolchain and GPU not available in this environment; build and functional results remain unverified.

### Added
- UV polygon coverage rasterizer with bounded work, reused by painting and flood fill.
- Optional UV coverage clipping of brush stamps and masks in 3D and UV views (on by default for supported meshes).
- Flood-fill tool with tolerance over the selected layer's active PBR channel, respecting UV coverage and integrating sparse Undo/Redo when within budget.
- CLI `uv-mask` for deterministic binary UV atlas coverage export and tests for winding, clipping, fill boundaries and connectivity.
- Optional `gpu-experimental` Cargo feature with first WGSL compute stamp shader and CLI `gpu-stamp`; **not connected to interactive painting**, uncompiled and hardware-untested.
- GPU compute design and validation checklist in `docs/GPU_COMPUTE.md`.

### Limitations and warnings
- The GUI still uses the CPU reference mesh rasterizer; enabling `gpu-experimental` does not accelerate the GUI.
- UV coverage is a binary triangle mask, not UV island identification, seam padding or dilation.
- Flood-fill matches texels in the selected layer, not the flattened visible composite, and is 4-connected.
- No live Rust tests or GPU execution were possible in this environment. Source inspection and archive validation are not compilation tests.

## v0.2.0-alpha.1 — 2026-10-10

Second source iteration, prepared for DrapNard. **Not compiled or runtime-tested in this sandbox** (Rust toolchain unavailable). Functional code below requires local verification.

### Added
- PNG/JPEG texture import as an independent editable layer on the active PBR channel; explicit Lanczos3 resize to project resolution.
- Native versioned, human-readable `.kbrush` brush presets (load/save, field validation and 64 KiB input cap).
- Fill/clear entire active channel, and fill black/white masks, as undoable document operations.
- Eyedropper for picking active-channel values from UV or 3D view.
- Headless CLI (`--version`, `--help`, `inspect`, `validate`, `export`, `preview`, `spp-inspect`, `brush-inspect`).
- Sparse 4 KiB chunk-diff paint history with a 96 MiB budget and 64-entry cap rather than retaining entire texture snapshots indefinitely.
- Input-safety checks for project manifest extra fields, duplicate IDs, duplicate channels and duplicate ZIP entries, unknown data and decoded texture budget (512 MiB).
- More unit tests and two native brush preset examples.
- `AUTHORS.md`, release naming and author attribution to DrapNard.

### Known limitations
- Same CPU mesh rasterizer, approximate PBR, and no native GPU compute strokes or UDIMs.
- `.spp` remains **signature detection only**; `.sppr`/`.sbsar` are not importers.
- Undo/redo applies to brush strokes and fill commands, not adding/removing/reordering layers, mesh import, resizing, or texture import.
- Large full-channel fills may exceed the undo history budget and thus not be undoable; 4K painting still creates temporary full-channel snapshots at the beginning/end of each brush stroke, although the retained history is sparse.

## v0.1.0-alpha — 2026-10-10

First Rust source prototype: egui/wgpu desktop shell, CPU reference 3D preview, UV hit-testing, mesh imports, 8 PBR channels, brush painting, masks, editable layer stack, native `.kalium` ZIP project, 11 PNG exports, stroke history and honest SPP detection-only inspector. Source was not compiled in the generation environment.

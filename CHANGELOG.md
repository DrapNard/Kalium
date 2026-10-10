# Kalium changelog

Kalium author and release identity: **DrapNard** (https://github.com/DrapNard). All changes distributed as AGPL-3.0-only.

## v0.2.0-alpha.5.2 — 2026-10-11

**Source hotfix under DrapNard.** A user build failed with `unclosed delimiter` at the `impl Layer` / `Layer::new` block in a 411-line local `src/document.rs`. The tagged alpha.5.1 source has only 250 lines and already has closed delimiters, indicating that this particular checkout contains additional local edits or a stale stash; simply fetching a new bundle does not replace local changes.

- Expand the `Layer::new` structure initializer into explicit lines and block delimiters, preserving behavior.
- Add a regression test for construction of a fully initialized, editable layer.
- Add a checkout-recovery guide that backs up local edits, fast-forwards to the new release, and verifies the exact checked-out version **before** any stash is re-applied.
- No feature parity with Adobe Substance Painter is claimed or introduced by this syntax-focused hotfix.
- Rust/Cargo are unavailable in this packaging environment. Git and packaging integrity can be verified, but compilation and test results must be obtained on a Rust-equipped machine.

## v0.2.0-alpha.5.1 — 2026-10-11

**Hotfix under the DrapNard release identity.** The user reported a Rust parser error (`unclosed delimiter`) around `KaliumApp::autosave_workspace()` after updating to alpha.5. The original alpha.5 bundle does contain a closed function, so the reported error could also be caused by older local changes or an applied stash. This hotfix makes the function explicit and flatter, eliminating deeply nested closing delimiters while preserving workspace saving behavior. Failure to encode the workspace now also reports an error in the status bar.

- Refactor `autosave_workspace()` to early returns with balanced, plainly formatted control blocks.
- Preserve the existing user layout and modified/unsaved project state on filesystem errors.
- Keep the v2 `.kalium` document manifest and all alpha.5 functionality unchanged.
- Document a **stash → fast-forward → test** upgrade path for clones with local edits. Do **not** reapply an old stash containing a broken `src/app.rs` over this version.
- Source/bundle packaging checks are performed, but Rust/Cargo are not installed in this delivery environment; compile/test confirmation is still required on the user's Arch machine.

## v0.2.0-alpha.5 — 2026-10-10

DrapNard source update, based on the 28/29 successful local unit tests reported from v0.2.0-alpha.4. **This update has not been compiled in the packaging environment.**

### Fixed
- `layout::tests::workspace_json_roundtrips`: egui_dock initializes not-yet-shown leaf/split rectangles with `Rect::NOTHING` (non-finite floats); JSON writes these as `null` even though `f32` deserialization rejects `null`. Only `min`/`max` coordinate pairs are normalized to finite zeroes, leaving legitimate optional `null` fields intact.
- Both workspace persistence and autosave now use the same safe serialization path; loading also repairs older JSON layouts with non-finite rectangles where possible, preserving docked/floating panel arrangement.
- Regression tests cover default layouts, floating panel window configuration and recovery of older JSON with `null` coordinates.

### Added
- Per-layer, per-PBR-channel enable/disable switches in the **Layers** pane (Base Color, Roughness, Metallic, Normal, Height, Emissive, Opacity, AO). Toggling a channel affects the visible composite and exported PBR maps without deleting any painted pixels.
- `.kalium` manifest **v2** stores `disabled_channels`; loader remains compatible with **v1** manifests, which default to all channels enabled. Disabling a channel is non-destructive and survives saving/reopening.
- Unit tests for channel compositing, project round-trip and v1 metadata defaults.

### Still missing
- Multiple mesh material texture sets, GPU viewport and interactive GPU strokes, advanced bake maps, smart materials, and `.spp` round-trips. The new channel switches are not a substitute for full Substance Painter functionality.

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

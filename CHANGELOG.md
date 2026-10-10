# Kalium changelog

Kalium author and release identity: **DrapNard** (https://github.com/DrapNard). All changes distributed as AGPL-3.0-only.

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

# Kalium architecture overview

Status is based on the current worktree inspected on 2026-10-18. The repository is a single Rust 2021 binary (`kalium`) rather than a workspace or published library. It combines an eframe/egui desktop editor with a headless CLI; both use the same document, import, project and export modules.

## Module map and data flow

```text
main.rs
  ├─ CLI args → cli.rs → project / brush_preset / export / spp / uv
  └─ no args → eframe → app.rs (TabViewer + editing interactions)
                         ├─ layout.rs (per-user DockState)
                         ├─ document.rs (mesh + active set + layers)
                         ├─ paint.rs / ops.rs → history.rs
                         ├─ uv.rs (bounded UV coverage raster)
                         └─ renderer.rs (CPU preview + UV/material hit buffers)

Mesh import → mesh.rs → Document::import_mesh
Document → project.rs (versioned ZIP/JSON/PNG) → reopen
Document → export.rs (per-set PBR PNGs)
```

| Area | Current owner | Contract and current limit |
|---|---|---|
| Desktop shell and workflow | `src/app.rs` | `egui_dock::TabViewer`; owns active tool/channel, file path, stroke lifecycle, refresh flags, dialogs, and panels. New/Open/native close use Save/Discard/Cancel guards. Layer rows support egui drag/drop reorder with stable selection and undo/redo; the interaction has synthetic pointer-event coverage but no desktop visual validation. |
| Workspace preference | `src/layout.rs` | Serializable `DockState<Pane>` stored under the user's config directory, separate from `.kalium` artistic content. Unknown/invalid layout falls back to defaults. |
| Document and compositing | `src/document.rs` | Mesh, active texture set, inactive sets, material bindings, raster layers and channel composites. Limits include 16 sets, 64 layers/set, 4096 texture edge, and a 512 MiB decoded budget. Base Color and Emissive brush/compositing use linear-light RGB; Normal brush/compositing uses normalized vectors, with an explicit Detail Normal layer mode using reoriented normal mapping. Scalar channels retain normalized data-value blend math. |
| Mesh import and validation | `src/mesh.rs` | OBJ and glTF/GLB import into indexed triangles with position, normal, UV0 and per-triangle material slot. Source bytes and assembled geometry are capped; external references remain inside the asset directory, and unused glTF images are skipped. glTF node transforms are baked into imported positions. Extra UVs, tangents, skinning and morph data are not retained. |
| Painting and tools | `src/paint.rs`, `src/ops.rs` | UV-space brush stamping/resampling, erasing, masks, sample/fill modes, undoable texture-layer import and connected flood fill. GUI interaction routes 3D picks or UV coordinates into this API. Pressure, seam projection and dirty-tile updates are not implemented. |
| Undo/redo | `src/history.rs` | One 96 MiB history covers tile-sparse interactive stroke deltas, fill snapshots, layer add/duplicate/remove/reorder, imported texture layers and coalesced layer properties. Brush strokes capture 64×64 touched tiles before mutation and store changed tile rows only; over-budget strokes are canceled and rolled back. Fill operations still snapshot and diff full images. |
| UV coverage | `src/uv.rs` | Bounded triangle-to-texture coverage raster used to clip paint and fill to selected material bindings. Single 0–1 tile; no UDIM addressing. |
| 3D preview and picking | `src/renderer.rs` | Deterministic CPU software rasterizer produces a display image, UV hit buffer and material-slot buffer. The preview shades linear-light materials with direct Cook–Torrance GGX, tangent normals and a constant ambient fill; numeric BRDF and deterministic frame tests cover the reference path. eframe uploads the image through wgpu. This is not native GPU rendering or GPU painting, and it has no IBL or controlled GUI image acceptance yet. |
| Native persistence | `src/project.rs`, `src/recovery.rs` | Versioned `.kalium` ZIP with JSON manifest and PNG layer/mask payloads; versions 1–6 are accepted; v5 adds Detail Normal blending, and v6 persists a stable project UUID for recovery association. Unknown JSON extension fields are retained at supported levels. Save writes a sibling temp archive before replacement; Open can recover only a fully validated temp when the primary is missing/invalid. Journal v2 validates project UUID + SHA-256 baseline + schema; the app durably appends bounded atomic batches for committed channel, fill and mask image edits and offers Restore/Discard on reopen. Other document mutations pause recovery via a sidecar marker until the next save. Journal writes synchronously flush on the UI thread; broad archive fuzzing and GUI prompt acceptance remain open. |
| Mesh-map baking | `src/bake.rs`, `src/cli.rs` | Same-mesh CPU world-normal and normalized-position maps can be emitted through a headless command. An internal high-to-low projection primitive requires low/high meshes already share coordinates; map persistence, alignment controls and generator use are not integrated. |
| Exports | `src/export.rs` | Eight channel PNGs plus base color/alpha, ORM, and Unity metallic/smoothness maps; the separate Base Color alpha map takes its alpha from the Opacity channel's red value. Supports active set and per-set folders with opt-in UV-aware padding. Bit-depth/rescale options and downstream-engine acceptance are absent. |
| Presets and compatibility | `src/brush_preset.rs`, `src/spp.rs` | Native `.kbrush` JSON presets. `.spp` performs signature identification only; no structural parser or import/export. |
| Experimental GPU compute | `src/gpu_brush.rs` | Optional `gpu-experimental` CLI stamp proof of concept. Not connected to interactive editing and not hardware-verified. |

## Important invariants

- Project content and per-user dock layout are separate persistence domains.
- Material bindings resolve per source triangle; missing bindings use the active texture set.
- Texture-set switching transfers owned buffers rather than cloning all texture data and clears undo history to avoid layer-ID collisions.
- Imported archives, models and images are untrusted. Project images and mesh source files have byte limits; OBJ/glTF decoder amplification and broad malformed-file fuzz coverage remain open.
- The official product has no generative AI/material/image feature and must not depend on network inference.
- The software rasterizer is the reference renderer. GPU-native viewport/painting is future work.

## Baseline evidence

On this Linux x86_64 worktree with rustc 1.97.1 / cargo 1.97.1:

- `cargo fmt --all -- --check`: passed.
- `cargo test --locked --all-targets`: 192 passed, 0 failed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo check --locked --all-targets` and `cargo check --locked --features gpu-experimental`: passed.
- GUI launch, interactive visual behavior, optional GPU feature, cross-platform builds and downstream-engine imports have not been verified in this session.
- `vulkaninfo --summary` could not create a Vulkan instance: the configured NVIDIA ICD could not load and the loader found no working drivers. Optional feature compilation is therefore not device validation.

The checks above were reproduced from an isolated clean clone at commit `9fc6f91`. GUI launch, GPU hardware and downstream application behavior remain unverified.

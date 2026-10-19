# Kalium architecture overview

Status is based on the current worktree inspected on 2026-10-19. The repository is a single Rust 2021 binary (`kalium`) rather than a workspace or published library. It combines an eframe/egui desktop editor with a headless CLI; both use the same document, import, project and export modules.

## Module map and data flow

```text
main.rs
  ├─ CLI args → cli.rs → project / brush_preset / export / spp / uv
  └─ no args → eframe → app.rs (TabViewer + editing interactions)
                         ├─ layout.rs (per-user DockState)
                         ├─ recent_projects.rs (bounded per-user recent paths)
                         ├─ document.rs / layer_tree.rs (mesh + texture sets + layer trees)
                         ├─ paint.rs / ops.rs → history.rs / recovery.rs
                         ├─ uv.rs (bounded 0–1 and UDIM-tile coverage raster)
                         ├─ renderer.rs (CPU reference PBR + UV/material hit buffers)
                         └─ gpu_viewport.rs (experimental WGPU Base Color preview)
  └─ procedural.rs (bounded Roughness graph core used by layer compositing)

Mesh import → mesh.rs → Document::import_mesh
Document → project.rs (versioned ZIP/JSON/PNG) → reopen
Document → export.rs (resampled/padded per-set PBR PNGs and profiles)
```

| Area | Current owner | Contract and current limit |
|---|---|---|
| Desktop shell and workflow | `src/app.rs` | `egui_dock::TabViewer`; owns active tool/channel, file path, stroke lifecycle, refresh flags, dialogs, and panels. New/Open/native close use Save/Discard/Cancel guards. Layer rows support egui drag/drop reorder with stable selection and undo/redo; the interaction has synthetic pointer-event coverage but no desktop visual validation. |
| Workspace preference | `src/layout.rs` | Serializable `DockState<Pane>` stored under the user's config directory, separate from `.kalium` artistic content. Unknown/invalid layout falls back to defaults. |
| Recent projects | `src/recent_projects.rs`, `src/app.rs` | Version-1 per-user JSON caps 12 paths, 4096 UTF-8 bytes per path and 32 KiB total. Synced atomic replacement precedes in-memory changes; unknown supported-version fields survive. Accepted primary opens and successful saves promote canonical paths. File → Open Recent reuses normal Open guards and project locking; missing files can be removed; recovery sources are excluded. |
| Document and compositing | `src/document.rs`, `src/layer_tree.rs` | Mesh, stable material-to-Texture-Set bindings and up to 16 independent sets, each with a recursive Paint/Fill/Group tree (up to 64 paint leaves). Full and region composites handle masks, visibility, opacity, channel switches and blend modes; Base Color/Emissive use linear-light math, Normal uses vector-aware blending and Detail Normal uses RNM. Seeded Roughness graphs can use same-set AO pixels and are evaluated during compositing. Texture edges are capped at 4096 and retained decoded images at 512 MiB. |
| Mesh import and validation | `src/mesh.rs` | OBJ and glTF/GLB import indexed triangles with material slots, baked node transforms, complete UV0, and retained glTF TEXCOORD_1–7 and optional tangents. UV0 remains the paint channel. Missing UV0, unsupported skinning and morph data fail with errors. Source bytes, node counts, selected-scene traversal, buffers and assembled geometry are bounded; external buffers are confined to the asset directory and unused images are skipped. Higher UV sets are preserved but not yet selectable for painting. |
| Painting and tools | `src/paint.rs`, `src/ops.rs`, `src/app.rs` | CPU brush stamping/resampling, paint/erase/mask modes, alpha tips, horizontal UV mirroring, fill/sample tools, texture-layer import and connected flood fill. 3D strokes use CPU surface hits and bridge verified shared edges across duplicated UV seam vertices; UV clipping uses material-set coverage. Interactive strokes update touched tiles and cached CPU preview regions, with sparse Undo/Redo and recovery. Pressure/tilt, general projection/seam painting and UDIM tile storage/routing are not implemented. |
| Undo/redo | `src/history.rs` | One 96 MiB history covers tile-sparse interactive stroke deltas, swap-based uniform Fill/Clear/Reset, self-inverse mask inversion, layer add/duplicate/remove/reorder, imported texture layers and coalesced layer properties. Brush strokes capture 64×64 touched tiles before mutation and store changed tile rows only; over-budget strokes are canceled and rolled back. Nonuniform Fill/Clear history retains the prior image buffer without a second full-image clone; Flood Fill remains a separate bounded-region path. |
| UV coverage | `src/uv.rs` | Bounded triangle coverage raster supports ordinary 0–1 UVs plus standard UDIM address/split and per-tile coverage helpers. Tile-address groundwork is tested, but actual documents, layers, painting, recovery, rendering and export still use one 0–1 tile per set. |
| 3D reference preview and picking | `src/renderer.rs` | Deterministic bounded CPU rasterizer produces the PBR display image and perspective-correct UV/material/surface hit buffers. Reference shading includes linear-light Cook–Torrance GGX, tangent normal maps, AO, emissive, analytic Studio/Warm/Cool environments and front-to-back opacity compositing (up to eight surfaces); CPU hit testing remains authoritative for model painting. Incremental tile refresh has byte-parity tests for all eight channels. HDR image-based lighting and broad desktop image acceptance remain open. |
| Experimental GPU mesh preview | `src/gpu_viewport.rs`, `assets/shaders/viewport.wgsl`, `src/app.rs` | Optional WGPU depth-tested preview routes triangles by bound Texture Set and samples that set's Base Color map with fixed diffuse/fill lighting. The app can update resident Base Color regions; scene rebuild is the fallback. The shader does not implement the CPU renderer's PBR channels/opacity. Hardware runtime was observed on Linux/Xvfb with Vulkan/NVIDIA, but GPU painted-region parity, broader visual quality and device recreation are not verified. GPU picking and editor GPU painting are not implemented. |
| Native persistence | `src/project.rs`, `src/recovery.rs`, `src/journal_writer.rs` | Versioned `.kalium` ZIP with JSON manifest and PNG layer/mask payloads; versions 1–9 are accepted and older versions migrate to v9. V5 adds Detail Normal, v6 stable project UUID, v7 bounded Roughness graphs, v8 AO graph inputs and v9 recursive per-set Paint/Fill/Group trees. Mesh tangents/extra UV sets and unknown JSON extension fields are persisted. Save uses a sibling temp archive and replacement; validated temp recovery handles missing/invalid primaries. Journal v2 validates project UUID + SHA-256 baseline + schema; bounded batches go through a byte/item-capped FIFO writer. Imported, generated, retained-layer and mesh replacement payloads use transaction markers; streamed snapshots support multi-record Undo/Redo and incomplete final transactions truncate before replay. Undo/Redo waits asynchronously for durable acknowledgments; file transitions and shutdown drain the writer. Broad archive fuzzing and desktop recovery acceptance remain open. |
| Mesh-map baking | `src/bake.rs`, `src/cli.rs`, `src/app.rs` | CPU same-mesh world-normal/position, material-ID and AO maps are available through CLI; AO and material-ID also have editor worker flows. High-to-low world-normal projection accepts an optional bounded affine high-mesh transform. AO can install as an editable layer and feed same-set Roughness graphs. Correspondence cages, curvature/thickness, dilation and project-persisted generated-map assets are not implemented. |
| Exports | `src/export.rs`, `src/app.rs` | Exports eight channel maps plus Base Color alpha, ORM, and Unity metallic/smoothness. Supports active/all sets, output resampling from 64 to 4096, optional UV-aware padding, OpenGL/DirectX normal Y, and Godot 4, Unity, glTF metallic-roughness and Painter Python API profile payloads. The Base Color alpha export uses Opacity red. Blender 5.2.2 import/render checks pass for a narrow fixture; Godot/Unity editor acceptance and non-PNG/bit-depth exports remain open. The Painter JSON payload is not `.spexp` shelf serialization. |
| Presets and compatibility | `src/brush_preset.rs`, `src/spp.rs` | Native `.kbrush` JSON presets include bounded alpha tips (v2 with v1 migration). `.spp` performs signature identification only; there is no structural parser, project importer, or writer. |
| Experimental GPU compute brush | `src/gpu_brush.rs`, `assets/shaders/paint.wgsl`, `src/cli.rs` | Optional `gpu-experimental` CLI proof of concept for a single round Paint/Erase stamp. It creates a GPU device per call, reads the full texture back synchronously, and is not connected to editor painting. Native GPU compute painting remains future work. |
| Procedural graph | `src/procedural.rs`, `src/document.rs`, `src/layer_tree.rs`, `src/project.rs` | Bounded deterministic seeded Roughness value-noise graphs are attached to Paint layers, composited with authored pixels/masks/opacity and persisted in `.kalium`; an AO graph input samples same-set source-layer pixels. The UI exposes graph controls and history/recovery preserve edits. This is a narrow graph foundation, not a general filter/generator/effects graph or Smart Material system. |

## Important invariants

- Project content and per-user dock layout are separate persistence domains.
- Material bindings resolve per source triangle; missing bindings use the active texture set.
- Texture-set switching transfers owned buffers rather than cloning all texture data and clears undo history to avoid layer-ID collisions.
- Imported archives, models and images are untrusted. Project images and mesh source files have byte limits; decoder amplification, coverage-guided fuzzing and peak-RSS profiling remain open.
- The official product has no generative AI/material/image feature and must not depend on network inference.
- The CPU software rasterizer is the reference renderer for full PBR shading, picking and painting. The experimental native WGPU mesh preview currently renders only depth-tested Base Color with diffuse lighting; GPU picking and interactive GPU painting are future work.

## Baseline evidence

Historical baseline from the clean-clone check at commit `9fc6f91` (not a current test result):

That historical run reported 194 passing tests. It predates the current implementation and must not be used as current verification. See the [QA test matrix](../qa/test-matrix.md) and [implementation status](../roadmap/implementation-status.md) for the current worktree's recorded evidence and remaining acceptance gaps. Current docs record an Xvfb/Vulkan/NVIDIA preview and narrow Blender import/render acceptance; they do not establish general GPU PBR, GPU painting, broad desktop workflows, or engine parity.

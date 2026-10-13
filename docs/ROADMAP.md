# Kalium roadmap — full Substance Painter-class target

This document describes **future development goals**, not current features. `v0.2.0-alpha.2` is a source prototype, not a complete v1.0 product.

## v0.1 — first editable foundation (this delivery)

- [x] Native Rust application, egui + wgpu GUI
- [x] Import OBJ/glTF/GLB mesh geometry; demonstration cube
- [x] Reference textured 3D preview + direct UV picking
- [x] Mouse brush/erase/mask, interpolated strokes, opacity and hardness
- [x] UV 2D canvas and optional mesh wireframe
- [x] 8 material channels
- [x] Layer stack (opacity, modes, masks, visibility, reorder)
- [x] Native editable ZIP project format
- [x] Basic undo/redo for brush strokes
- [x] PNG and packed PBR exports
- [x] Unit tests for basics (not run in this environment)
- [ ] Confirm build on Linux/macOS/Windows and fix any integration issues

## v0.2-alpha.1 — source iteration (2026-10-10)

- [x] Native editable brush presets `.kbrush` (not `.sppr`)
- [x] Texture PNG/JPEG imported as layer
- [x] Fill/clear channel and mask with history
- [x] Bounded sparse stroke/fill history (still captures dense temporary before/after buffers)
- [x] Headless inspect/validate/export/preview/preset-inspect CLI
- [x] Reject project archives with unrecognized schema additions and oversize resources
- [ ] Confirm build and runtime tests on supported platforms

## v0.2-alpha.2 — UV-aware painting and experimental compute (2026-10-10)

- [x] Rasterized, bounded UV coverage mask with mesh triangle clipping in 2D/3D strokes
- [x] 4-neighbor flood-fill with color tolerance and UV-boundary protection
- [x] PNG UV coverage visualization from the CLI (`uv-mask`)
- [x] Opt-in WGSL compute brush proof-of-concept and host code (CLI-only, unverified)
- [ ] Compile and compare compute stamps to CPU reference on real Vulkan/Metal/DX12 hardware
- [ ] Integrate long-lived GPU buffers and GPU brush commands with the interactive viewport

## v0.2 — fast production core

- [ ] GPU-native wgpu 3D mesh viewport, depth/stencil picking, GPU painting compute path
- [ ] Shader hot reload, bindless-style or atlas texture binding strategy
- [ ] Work on 4K–8K assets without full texture recomposition each stroke; incremental dirty tiles
- [ ] Tangent basis and proper metallic/roughness PBR and image-based lighting
- [ ] GPU brush stamps, texture tiling, pressure/tilt input, brush presets and alpha masks
- [ ] Brush symmetry in model space, UV-edge projection, painting over seams
- [ ] Native async jobs and cancellation
- [ ] Correct color-space pipeline (sRGB/linear, EXR, 16/32-bit)
- [ ] Memory safe undo delta tiles, save recovery, backups

## v0.3 — 3D material workflow

- [ ] Multiple meshes and material texture sets
- [ ] UDIM tiles, virtual texturing and texture streaming
- [ ] Mesh/UV data inspection, baking viewport and UV island selections
- [ ] PBR baking: AO, bent normal, curvature, thickness, position, world normal, ID, normal from high-poly
- [ ] Ambient environments and custom material viewer shaders
- [ ] GPU mask generators and reusable procedural filter graph

## v0.4 — artistic parity

- [ ] Parametric fill layers, blur/sharpen/levels/HSL/curves and anchor points
- [ ] Smart Materials and Smart Masks
- [ ] Stencil painting, clone tool, projection, polygon fill, particle brushes and decals
- [ ] Material/scattering brush dynamics, tablet orientation and accurate stamp spacing
- [ ] Asset shelf with search and tagging
- [ ] Full layer-fx, per-channel blending, grouping, instances and non-destructive filters
- [ ] Export templating, batch CLI, plugins with stable C ABI and WASM sandbox

## v0.5 — interoperability research

- [ ] Clean-room `.spp` container/HBO parser; corpus with multiple Painter versions
- [ ] Lossless import of recognized SPP data without destructive flattening
- [ ] SPP editable writer with Painter re-open tests and unknown-block preservation
- [ ] `.sppr`, `.spsm`, `.spmsk`, `.spexp` import/export
- [ ] Adobe `.abr` brush import
- [ ] `.sbsar` runtime: either compatible independent evaluator or documented limitations; never assume free decoding gives executable Substance graphs
- [ ] Automated visual comparison of strokes and materials against lawfully captured reference renders

## v1.0 — community-quality release gate

- [ ] Documented per-feature compatibility matrix and tested sample-corpus results
- [ ] Crash-safe transactional saves, file fuzzing, sanitizers, resource usage regression tests
- [ ] Accessibly localized UI, scalable panels, keyboard/tablet navigation and Wayland parity
- [ ] Linux, Windows and macOS builds and CI artifact signing
- [ ] Stable file format migrations and project recovery
- [ ] Profiling budgets for common asset sizes and hardware tiers
- [ ] Security review of parsers, plugins and remote content

No unverified feature may be marked complete merely because a button or API surface exists. Reliable use matters more than menu parity.

## Texture Set milestone — 2026-10-11

The alpha.6 milestone is implemented as v0.3.0-alpha.1 source: independent per-set layers, resolution, persistence and exports. **Follow-up milestone:** texture-to-material binding completed at source level in alpha.2; native GPU PBR and seam-aware painting remain open.

## Material binding milestone — 2026-10-13

Kalium v0.3.0-alpha.2 adds retained OBJ/glTF material slots, a stable material-to-Texture-Set binding table, per-triangle CPU multi-material preview and material-aware UV hit testing and brush clipping. These features are implemented in source and still require a Cargo build/test pass on the maintainer's Arch machine. **Next milestone:** native GPU mesh viewport with physically based lighting, persistent texture resources and UV seam-aware painting.

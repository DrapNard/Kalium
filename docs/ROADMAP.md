# Kalium roadmap — full Substance Painter-class target

This document describes **future development goals**, not current features. `v0.1.0-alpha` is a playable source prototype, not a complete v1.0 product.

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

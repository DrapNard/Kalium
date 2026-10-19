# Kalium and SubCraft integration

Owner request, 2026-10-19: analyze both repositories and combine their strongest
parts in one application, rewrite the README, commit and publish the result.
The initial experiment used Kalium as primary. The owner subsequently chose
Subcraft as the main app/repository; this branch preserves the experiment only.
SPP-first development
continues; SubCraft contains no SPP importer or writer.

Reference snapshot: `DrapNard/subcraft` commit
`71e49fde7e4e1dba188a6589c9e5a139d1e3d9d5`, forked from Aalzard/SubCraft.
MIT notices are in [THIRD_PARTY_NOTICES.md](../../THIRD_PARTY_NOTICES.md).

## Source comparison and selection

| Area | Kalium | SubCraft | Combined direction |
|---|---|---|---|
| Application | Rust 2021, egui 0.31, wgpu 24, functional docked editor | Rust 2024 workspace, egui 0.36 | Keep one Kalium executable and existing build architecture |
| Project safety | Bounded archive preflight, decoded-memory/tree caps, unknown fields, journals and recovery | Rich schema/resources/maps; loader reads archive sizes without equivalent aggregate caps | Keep Kalium persistence/recovery; do not transplant the loader |
| Compositing | Recursive groups, eight channels, linear color, normalized normals, sparse history | Wider arithmetic/color/normal blend family | Adapt blend math into the live Kalium recursive compositor |
| GPU preview | Depth-tested Base Color preview, CPU authoritative | Metal/rough GGX, multi-channel shading, procedural environment, tone mapping | Adapt open-source shading and channel uploads to existing wgpu API |
| Baking | Bounded cancellable surface raster/BVH, AO, world normals, position, ID | Additional 3D-neighbourhood curvature and inward-ray thickness | Adapt these algorithms to Kalium caps, bindings and CLI |
| UI/content | Painter-style docks and controls, functional existing actions | Larger shelf, guides, more tools and starter images | Preserve Kalium UI; assess tools individually without bundling an unreviewed duplicate app |
| SPP | Controlled private oracle research; no semantic reader/writer | No SPP parser | Continue the standalone editable SPP contract/round-trip work |

## Integration requirements

- Existing projects, enum spellings, recovery/history and CPU reference behavior
  must keep working. New modes must survive save/reopen and journal replay.
- All eight material-channel updates must reach the GPU preview, including
  dependent procedural Roughness changes after AO painting.
- Additional bakes must reject invalid/non-finite options, respect shared memory
  and work budgets, honor Texture Set binding and cancellation, and preserve
  unpainted atlas space. Outputs are standalone maps until persisted-map support.
- Shader validation is evidence of syntax/types only. Rendered acceptance needs
  actual GPU output; alpha cutout does not establish layered transparency parity.
- No starter images, third-party fonts/icons, Adobe resources or generated art
  are introduced by this integration. Original source attribution is retained.

## Remaining differences

This source integration does not import SubCraft's entire session/document/UI
architecture. Its pen paths, part workflows, projection modes, HDRI processing,
smart-material library, per-channel blend overrides and tiled float storage need
separate adapters and acceptance. Both native formats remain distinct; neither
is SPP. GPU paint and picking remain future work. Do not describe this merge as
full Painter or full SubCraft feature parity.

## Experiment checkpoint

Initial integrated run: 628 passed, 2 failed, 2 ignored. Failures were an incorrect
HSV test expectation and missing channel allocation in the new roundtrip test;
both were corrected but not rerun before switching to the owner-selected
Subcraft destination. Do not treat this branch as accepted production integration.

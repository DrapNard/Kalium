# QA test and acceptance matrix

Updated 2026-10-18. Results below were reproduced from isolated clean clone `9fc6f91`; GUI and hardware acceptance remain separate.

## Commands run

| Command | Result | Scope/notes |
|---|---|---|
| `cargo fmt --all -- --check` | PASS | Current Rust files are formatted. |
| `cargo test --locked --all-targets` | PASS: 192 passed, 0 failed | Includes save-choice/recovery policy, checkpoint primary-path collision detection, manual checkpoint round-trip preserving primary bytes, pre-decode image limits and metadata validation, unknown JSON extension round trips, duplicate/unknown ZIP entry rejection and decoded-budget preflight, bounded OBJ/glTF inputs, glTF image skipping/path confinement/TRS baking, transformed glTF pick/paint/save/reopen/export, texture resize budgets, linear-light Base Color and Emissive brush/compositing oracles, Normal brush interpolation/renormalization and Detail Normal RNM, tile-sparse 4K stroke capture/undo, exact channel/mask undo/redo, over-budget stroke rollback preserving redo, undoable imported-layer restoration, mixed layer-stack/property/pixel undo-redo, history budgets, egui layer-row drag/drop reorder with undo/redo and stable selection, packed export assertions, OpenGL/DirectX Normal byte oracle, TwoMaterials workflow, analytic/transformed picking, near-plane clipping, mirrored-UV normals, stroke interpolation, UV padding and temp recovery, GGX/deterministic frame oracles, material-map refresh, incremental composite-region equality across channels/layer semantics and cached paint-tile patch equality, mesh-bake interpolation/projection, deterministic bounded AO plane/blocker/cancellation checks, extreme-UV bake/coverage safety, recovery journal UUID/digest/schema/checksum/sequence/size/truncation tests, event-batch size/unknown-variant rejection, active/inactive-set patch replay and malformed-target/budget-boundary checks, channel/mask changed-tile and snapshot patch extraction, app save→journal append→reopen→restore for stroke/fill/mask/layer-property/order/selection edits, oversized stroke fail-closed behavior, torn-tail prefix restore and append, Discard that resets to an empty journal, and fail-closed behavior for unsupported document mutation. |
| `cargo clippy --locked --all-targets -- -D warnings` | PASS | One module-level dead-code allowance is scoped to the internal high-to-low prototype, which is intentionally not exposed until mesh alignment is persisted. |
| `cargo check --locked --offline --all-targets` | PASS | Linux host compile check and confirms current manifest/lockfile consistency without network access. |
| `cargo check --locked --features gpu-experimental` | PASS | Optional compute module compiles; not graphics-device validation. |

## Acceptance coverage

| Workflow/area | Current evidence | Missing evidence / next acceptance work |
|---|---|---|
| Mesh import + material slots | OBJ material/path boundary tests; validation and primitive-index tests; source/aggregate/depth bounds; glTF unused-image and TRS-baking fixtures; TwoMaterials multi-set painted/exported core workflow | Malicious file corpus, decoder amplification analysis, glTF material/transform edge cases, extra attribute preservation, GUI import evidence |
| Picking + UV | UV coverage; analytic pixel→UV/material interpolation, nearest overlap, transformed/nonuniform-scale, near-plane crossing, fully-behind/degenerate rejection and mirrored-UV normal handedness | Importer transform preservation, seams and broader geometry fuzzing |
| Continuous paint + undo | CPU stamp/clipping, sparse 64×64 tile capture at 4K, repeated tile capture once, absent-channel restoration, mask undo/redo, over-budget cancellation rollback preserving redo, disconnected-jump partial-opacity and connected interpolation tests; linear-light Base Color/Emissive stamp oracles; mixed paint/layer removal undo/redo | GUI down/move/up single stroke, same visible result in both views, seam behavior, GUI cancellation, paint-latency measurements and interactive undo/redo |
| PBR/layers/masks | Numeric Base Color/Emissive linear-light blend tests, weighted Normal interpolation, Detail Normal RNM with alpha/opacity/mask weighting, channel/layer/mask and fill tests; GGX BRDF oracles and a fixed center-pixel tolerance for a deterministic 200×200 CPU reference render | Complex nested/group mask fixtures, live GUI visual check and broader controlled PBR render images |
| Save/reopen | v6 multi-set/binding/layer and stable UUID roundtrip, v4→v6 migration, v1 fixture with generated identity, two-material workflow; unknown metadata round trips; pre-decode dimension/metadata limits; duplicate/unknown ZIP rejection; valid primary/temp recovery and injected replace failure tests; separate per-user checkpoint roundtrip leaves primary unchanged; recovered UI state requires Save As | Checkpoint menu GUI interaction, app event emission and startup journal recovery prompt, background save, process-kill recovery, broader migrations/corruption/property/fuzz tests |
| Exports | Pixel assertions for ORM/per-set directories, nearest-source Euclidean oracle, edge padding and set isolation; radius zero byte-equal to legacy; OpenGL/DirectX Normal Y conversion only changes G. Blender 5.2.2 smoke imports `TwoMaterials.obj`, loads Base Color/ORM/Normal/Emissive exports, verifies Principled node links and saved `.blend` reopen. | Bit-depth/rescale, named engine presets, Godot/Unity import, rendered appearance, and a different exported map per material slot/set |
| Unsaved document transitions | Modal Save/Discard/Cancel policy tests; recovered opens detach the primary save path and remain marked unsaved | GUI interaction and failed/canceled save handling need runtime GUI evidence |
| GPU / visual rendering | Headless renderer unit tests | GUI screenshot/runtime, device recreation, optional compute output and supported hardware validation |
| Mesh-map baking | Analytic CPU fixture verifies interpolated position and world normal; material binding filter, overlap ownership, cancellation, size and binding-count checks; demo CLI outputs world-normal/position/coverage PNGs and metadata | High-to-low projection/cage, AO/ID/curvature/thickness, dilation, persistent mesh-map storage, generator use and GUI progress/cancel |
| Release/platform | Linux format/lint/build/test commands; CI workflow now contains fmt/clippy gates and parses as YAML; Cargo lock consistency confirmed with offline locked compile | Remote CI run, GUI startup, Windows/macOS builds, packaging, license scan, reproducible install/uninstall and recovery |

GUI launch was attempted through Wayland, X11, and Xvfb but could not connect because no compositor or X server was available. Xvfb could not bind its socket directory in the restricted environment. `vulkaninfo --summary` found no working Vulkan driver (configured NVIDIA ICD failed to load), and no software Vulkan ICD is installed. No Kalium graphics capture or performance benchmark was performed. A real Blender import smoke passed; it checked map loading, node wiring, mesh material assignment and `.blend` save/reopen, but did not render or judge appearance. Reproduce it with:

```bash
cargo run --locked -- export-all examples/Kalium-Demo.kalium /tmp/kalium-downstream
blender --background --factory-startup --python-exit-code 1 \
  --python scripts/qa/blender_import_smoke.py -- \
  /tmp/kalium-downstream/001_Default examples/TwoMaterials.obj \
  /tmp/kalium-blender-smoke.blend
```

The smoke assigns one exported set to all imported OBJ slots. Different per-slot texture sets, Blender render appearance, and Godot/Unity imports remain unverified.

# Session handoff

## Checkpoint

- Date: 2026-10-18
- Branch: `main`
- Verified implementation commit: `28a5555` (`feat: integrate painting recovery and bake workflows`)
- Verification checkpoint commit: `7883e51` (status and handoff update above the implementation commit).
- Initial state: ahead of `origin/main` by 2 commits; dirty changes in 16 tracked Rust files; untracked `Cargo.lock` and `docs/roadmap/master-goal.md` already present before this checkpoint.
- New documentation created in this session: `docs/architecture/overview.md`, `docs/roadmap/implementation-status.md`, `docs/roadmap/decisions.md`, `docs/roadmap/agent-workboard.md`, `docs/roadmap/session-handoff.md`, `docs/research/reference-evidence.md`, and `docs/qa/test-matrix.md`.
- Commit `12bc283` is the reviewed S0-003 slice containing only `.github/workflows/ci.yml`, `Cargo.toml`, and `Cargo.lock`. Commit `28a5555` integrates the reviewed source, tests, QA, and progress documentation.
- This continuation adds the manual checkpoint API/UI, CPU GGX reference and its numeric/pixel tests, the Blender smoke script, and corresponding docs. The branch remains dirty and uncommitted; no files were staged.
- The current continuation adds `src/bake.rs`, a same-mesh `bake-mesh-maps` headless command, a shared-coordinate high-to-low projection primitive, a bounded `src/recovery.rs` journal framing module, regression tests and current S8/S13/QA audit findings. All changes remain in the shared dirty worktree; no files were staged or committed.
- This continuation integrated S5 image-event journaling, followed by selected layer structural events. Saves establish a UUID/digest-bound sidecar; committed image edits and supported layer property/order edits append bounded, synced batches; opening offers Restore/Discard; unsupported document changes create a disabled marker so partial recovery is not presented. Headless reopen tests cover image, layer-property/order replay and Discard. The 1-MiB raw patch-event budget and synchronous UI-thread sync remain limitations. Layer create/delete, set/mesh/binding, undo/redo events, desktop interaction and process-kill evidence remain open. The suite then had 186 passing tests.
- Current dirty paths are tracked in `git status --short`; Cargo.lock and the workflow/manifest change are committed. The current implementation includes layer-row DnD, `Document::composite_region_for_set`, material-map region patching, and partial UV texture updates for touched 64×64 paint tiles.
- Isolated clone at `7883e51` is clean and passes `cargo fmt --all -- --check`, all 186 baseline tests, strict Clippy, GPU-experimental feature compilation, and `git diff --check`. This verifies the committed formatting/lint cleanup in `28a5555`. Remote CI remains unavailable because origin is a local bundle.
- The latest source integration adds AO CLI output and closes two recovery gaps: selected-layer replay is journaled, and an oversized committed patch disables recovery rather than exposing an incomplete older prefix. Regression tests cover both. The integrated suite was 191/191 at that commit. The `bake-ao` demo smoke writes a 512×512 RGBA PNG with 262,144 covered texels; AO output is not saved to projects.
- Commit `9fc6f91` integrates the AO CLI and recovery fixes. A fresh isolated clone at that commit passed formatting, 191 tests, strict Clippy, GPU-experimental feature compilation, diff checks and clean status. Commit `1316e71` adds app-level torn-tail recovery; its 192-test run was from the main worktree. Hosted CI remains unavailable because origin is a local bundle.

- Current follow-up changes add `restore_recovers_valid_prefix_after_torn_final_journal_frame` in `src/app.rs`. It truncates a simulated final frame, confirms Restore applies the complete visibility event but not the incomplete opacity event, then verifies the journal accepts a later append. Focused test and full suite pass. Commit `88c5ea1` includes complete-frame checksum rejection and passed clean-clone checks with 193 tests. Commit `1316e71` added app-level torn-tail restore/append; commit `88c5ea1` added complete checksum-corrupt frame rejection and passed clean-clone gates at 193 tests. Current follow-up adds an end-to-end AO CLI PNG test; 194 tests pass locally and clean-clone verification is pending.

## Stage/task status

- S0–S8 remain `IN_PROGRESS`; unit checks pass, but acceptance evidence is incomplete. S9–S11 are `NOT_STARTED`; S12–S15 are `IN_PROGRESS`. See implementation status for details.
- UI-001 is implemented and code-reviewed, with 2 policy tests; GUI interaction remains unverified.
- S5-001 and S1-001 image decode bounds are implemented and verified by regression tests.
- S1-002 headless TwoMaterials paint/save/reopen/export workflow is verified by pixel assertions.
- S1-002 now includes actual CPU viewport material-slot picks, hit-UV coverage checks, rerendered paint visibility and the existing per-set save/reopen/export pixel assertions.
- S2-001 analytic pixel→UV/material and overlap tests are verified in the full suite.
- S0-002 CI fmt/Clippy steps are added; S0-003 tracks Cargo.lock and uses `--locked` in commit `12bc283`. Locked clean-clone checking and tests pass; formatting and strict-Clippy cleanup are integrated and clean-clone verified; remote CI is unavailable.
- S2-002 near-plane clipping is integrated and verified as a test slice: one/two vertices behind remain pickable with ray-oracle UV checks; fully-behind triangles produce no hits.
- S2-003 fixes mirrored-UV tangent handedness and tests normal-map orientation plus degenerate-triangle rejection.
- S4-001 adds preflighted resize budgeting across active and inactive texture sets; its focused and full-suite tests pass.
- S4-002 composites Base Color in linear light and Normal with normalized vector interpolation; S4-003 extends linear-light layer compositing to Emissive. S3-003 extends straight-alpha linear brush stamping to Emissive. S3-004 adds normalized Normal brush stamps, S4-004 adds RNM Detail Normal, and S4-005 makes imported texture layers undoable without clearing prior history.
- UI-002 implements bounded undo/redo for add, duplicate, remove, reorder and layer properties alongside pixel edits. Removed payloads restore masks, channels and properties; add/duplicate redo uses recipes. Property edits coalesce. The exact history payload cap boundary is tested.
- UI-003 / S4-006 adds layer-row drag/drop reorder through egui. A headless pointer-event test verifies a rendered-row drag, stack order, stable selection, dirty state, undo and redo; desktop GUI behavior remains unverified.
- S3-002 paints Base Color with linear-light straight-alpha source-over; three numeric brush tests pass.
- S1-004 adds a transformed glTF render/pick/paint/save/reopen/export workflow with an embedded fixture.
- S6-002 pins packed ORM, Unity metallic/smoothness and opacity-derived base-alpha output bytes.
- S5-005 adds manifest v6 project UUID persistence; v4→v6 migration/reopen and nil-UUID rejection tests pass. S5-004 adds a validated manual checkpoint save/load API, a per-user storage location, and File-menu save/recovery actions. Round-trip testing covers layers, masks, multi-set pixels, bindings and unknown metadata while asserting the primary archive remains byte-identical. Timed autosave and background snapshots remain unimplemented.
- The checkpoint UI review confirmed the existing unsaved Cancel/Save transition paths and found the checkpoint is intentionally global across projects; the menu now labels it global, and saving is rejected if the checkpoint path is also the active primary project.
- S7-001 adds a deterministic CPU reference Cook–Torrance GGX shader with linear-light Base Color/Emissive and analytic numeric oracles. It remains a fixed-light CPU reference, without IBL, studio controls or GUI image acceptance.
- S7-002 adds a rendered-frame regression proving a committed Base Color map edit appears after the viewport rebuilds its material snapshot. This is a renderer regression, not end-to-end GUI acceptance.
- S8-001 adds a standalone same-mesh CPU bake command for world normals and normalized position with UV coverage and encoding metadata, plus an internal tested high-to-low normal projection primitive requiring already aligned mesh coordinates. Neither result is saved in `.kalium`; mesh alignment/cage transform, additional maps and generator integration remain.
- S3-005 replaces full-image per-stroke snapshots with 64×64 touched-tile capture. A 4096² fixture verifies that one small dab captures less than 40 KiB of projected before/after data, repeated dabs capture each tile once, and undo/redo restore exact channel and mask bytes. Strokes exceeding the 96 MiB history budget roll back without clearing prior undo/redo. S13-006 recomposites unique touched regions for active paint and patches the cached map and UV texture; region output and patched cache bytes match full composites. Fill/clear and structural edits still use full invalidation, and viewport rendering remains full-frame CPU. These are code/test findings, not latency benchmark results; `perf`, heaptrack, valgrind and GUI profiling are unavailable here.
- S6-004 adds a Blender 5.2.2 import/node-link/save-reopen smoke script; it verifies exported map loading and graph wiring, not rendered appearance.

## Verification run

- `cargo fmt --all -- --check` — PASS.
- `cargo test --locked --all-targets` — PASS, 194 passed / 0 failed locally; commit `88c5ea1` passed clean-clone fmt, all 193 tests, strict Clippy, GPU-experimental compilation and diff checks.
- `cargo clippy --locked --all-targets -- -D warnings` — PASS after stroke recovery patch extraction and replay validation.
- `cargo fmt --all -- --check` and `git diff --check` — PASS after manifest-identity integration.
- Earlier in this worktree, `cargo check --locked --offline --all-targets` and `cargo check --locked --all-targets --features gpu-experimental` passed (compile only).
- `cargo clippy --locked --all-targets -- -D warnings` — PASS.
- `cargo check --locked --offline --all-targets` — PASS.
- `cargo check --locked --all-targets --features gpu-experimental` — PASS (compile only).
- `cargo run --locked -- --version`, validate demo, preview demo, inspect brush, and generate UV mask — PASS; output artifacts were written under `/tmp`.
- `cargo run --locked -- export-all examples/Kalium-Demo.kalium /tmp/kalium-downstream` — PASS; exported 11 maps for the Default set.
- `cargo run --locked -- bake-mesh-maps examples/Kalium-Demo.kalium /tmp/kalium-bake-smoke-final` — PASS; emitted 512×512 world-normal, position and coverage PNGs plus JSON metadata for 262,144 covered texels; Python/Pillow verified file dimensions and metadata.
- `blender --background --factory-startup --python-exit-code 1 --python scripts/qa/blender_import_smoke.py -- /tmp/kalium-downstream/001_Default examples/TwoMaterials.obj /tmp/kalium-blender-smoke.blend` — PASS on Blender 5.2.2; verified PBR map load, Principled links, OBJ material slots, and saved `.blend` reopen. Blender logs read-only extension-cache cleanup warnings in this restricted environment; the smoke assertions pass and command exits 0.
- GUI launch was attempted using Wayland, X11 and Xvfb. There is no compositor or X server; sandbox permissions prevent Xvfb from binding `/tmp/.X11-unix`; Vulkan has no working driver/software ICD. No GPU visual/performance test or Kalium GUI acceptance was run.
- The autosave architecture review found that a deep document clone would block and duplicate up to the decoded-document cap, while current mutations are too scattered to journal completely. A stable project UUID is persisted in manifest v6, Save As forks the UUID, and journal v2 headers validate UUID + SHA-256 baseline digest + schema. App journaling now covers committed channel/fill/mask image changes and has Restore/Discard recovery on reopen. Unsupported edits block replay via a sidecar marker. Structural events, background writer, GUI runtime evidence and crash-kill automation remain open; timed snapshot autosave remains disabled.
- Manual checkpoint review found no project identity or timestamp in the global checkpoint. The menu now calls it global, but the UI still does not show which project created it or ask for confirmation before replacing a clean current project. Repeated replacement behavior on Windows has not been verified.

## Risks / open issues

- Existing dirty implementation is substantial and was not comprehensively reviewed by the passing test run alone.
- Native save uses a sibling temp file and Open can recover a fully valid temp archive. Manual recovery checkpoints are available under the user config directory. The bounded journal is integrated for image edits with a Restore/Discard prompt, while broader mutation events, background syncing, desktop modal validation and process-kill evidence remain open.
- Texture resolution changes now preflight the full decoded-byte budget before allocation and leave inactive sets untouched; structural edits disable the current journal until the user saves a new baseline.
- Fuzz coverage for decoder and project archive edge cases remains incomplete.
- Fill/clear history remains full-image. Active brush strokes patch touched map regions; fills, structural/property changes, and cache misses still rebuild full composites. Stroke latency and whole-app memory behavior have not been benchmarked.
- The composite output alpha remains opaque; `Kalium_BaseColor_Alpha.png` uses Base Color RGB and Opacity-channel red as its alpha, with a byte-level test. Emissive brush/layer blending, Normal vector stamps, Detail Normal RNM composition and scalar data-value blending are tested; downstream expectations remain open.
- CPU reference renderer now uses deterministic direct GGX Cook–Torrance shading, but is not native GPU PBR and has no IBL, configurable lighting controls, or GUI image acceptance. The glTF TRS bake fixture covers one transform case; broader transform edge cases, seam picking and geometry fuzzing still need tests.
- Validated temp recovery and image-edit journal recovery are integrated; structural event coverage, desktop prompt validation and process-kill recovery evidence remain absent.
- No GUI-driven import → pick → paint → UV preview → save/reopen → export workflow yet.
- Blender 5.2.2 import smoke verifies exported map loading, Principled node links, and `.blend` save/reopen. It does not render-check appearance or verify distinct maps per OBJ material slot. Reproduce via `docs/qa/test-matrix.md` and `scripts/qa/blender_import_smoke.py`.

## Next three concrete edits

1. Wire committed brush/fill/mask patches into bounded journal records, then cover structural, set and mesh mutation events before enabling startup restore/keep/discard recovery UI.
2. Benchmark 1K/2K/4K paint strokes and validate egui partial texture updates in a desktop session; the region compositor is covered by byte-equality tests but has no latency measurement.
3. Keep the CI command set green after each code slice; the current verified clean clone is `7883e51`. The Cargo.lock is tracked since `12bc283`; remote CI is unavailable because the configured origin is a local bundle.

REA/Ghidra evidence: none used. Legal/source review was unnecessary for these ordinary engineering tasks.

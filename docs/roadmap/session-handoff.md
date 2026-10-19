# Session handoff

## Checkpoint

- Date: 2026-10-19
- Branch: `main`
- Current working checkpoint: HEAD is `f3593d7`; the shared worktree has uncommitted implementation, QA and roadmap changes. `.git` is read-only in this environment, so changes remain unstaged and uncommitted.
- Latest continuation (2026-10-19): S4 now stores recursive Paint/Fill/Group trees in Documents and Texture Sets, composes nested groups with value-plus-coverage semantics, persists manifest v9 with v1–v8 migration, and records stable-ID tree history/recovery through bounded subtree transactions. The headless Layers-pane workflow covers Add Group/Fill, edits, pointer reparenting into/out of Groups, rejected cycles with visible status, Save/reopen, tree-move Undo/Redo and journal Restore. Streaming preflight shares extension metadata budgets across manifest scopes and caps layer names. Fill/Group spatial mask strokes now use sparse history and stable-ID recovery, with full cache invalidation; deterministic full-frame CPU PBR fixtures check material parameter response; and S4-009 duplicates Paint/Fill/Group subtrees with fresh IDs and bounded crash recovery. The full suite passes 529 tests / 0 failed / 2 ignored; strict Clippy, `gpu-experimental` compilation, formatting and diff checks pass. The tagged alpha.2 ZIP, Git bundle and checksums also pass the clean-clone source-artifact smoke. Desktop acceptance, hosted CI, and hardware/GPU acceptance remain open; all S0–S15 stages remain IN_PROGRESS.
- Latest continuation fix: layer-row clicks now route through stable layer IDs, finish any pending stroke, mark the serialized selection dirty, and append `LayerSelection` recovery events. A synthetic pointer click test verifies selection save/reopen and journal Restore; the existing drag-reorder selection test still passes. Full suite: 487 passed / 0 failed / 2 ignored. Strict Clippy, GPU-feature compilation and offline `cargo package` pass; Cargo packaged and verified 63 files. Native desktop input, Windows/macOS and hosted CI remain unverified.
- New S13-010 slice: the CPU reference PBR viewport samples Opacity and composites up to eight nearest surfaces front-to-back in linear light. A transparent front surface reveals rear geometry while the nearest surface remains the paint target; solo Opacity displays raw channel values. Opacity edits and channel refreshes in a visibly transparent PBR frame use full rendering because the previous incremental cache retains only nearest-surface shading inputs. Viewport construction is clamped to 1100×900. Overlap, picking, solo-channel, deterministic output, layer-cap, cache-fallback and app-level full-render parity tests pass. Parent independently reran all-target tests (487 passed / 0 failed / 2 ignored), strict Clippy, formatting/diff checks and GPU-feature compilation. Eight-layer truncation and desktop visual acceptance remain.
- Historical S4 planning note (superseded): the previously isolated `LayerTree` has since been integrated into Document/TextureSet, v9 persistence/migration, tree-aware history/recovery, compositor and headless group/fill UI. Remaining acceptance is broader nested-tree gesture coverage and native desktop compositing review.
- Current implementation slices: project loads reject archives above 1 GiB and preflight ZIP/ZIP64 directory allocations before archive parsing; imported, generated and restored channel layers use bounded batches inside crash-atomic transactions; startup truncates incomplete transactions and preserves earlier edits. glTF import chooses the first scene when no default is declared, corrects winding for reflected transforms, and preserves TEXCOORD_1–7 in mesh data (UV0 remains the active paint channel). S5 recovery journals now protect untitled sessions through a per-user validated baseline plus supported edit events; saved-project journals cover image, layer, Texture Set (including active-set resize), material binding, property, selection, Undo/Redo and mesh replacement edits. Full retained layer payloads for removed-layer Undo and imported/generated-layer Redo replay through ordered snapshot transactions that preserve masks, channels, properties, extensions, selection and stable IDs; history streams 256-KiB chunks into bounded records without materializing the full snapshot event list. Mesh bytes and exact material bindings are staged, SHA-256 checked and committed atomically in a 32-MiB/4-MiB-chunk transaction. S5-025 now routes app journal writes through a bounded FIFO worker; Undo/Redo remains pending until all journal tickets confirm durable sync, and save/open/discard/shutdown drain the writer before replacing or deleting its file. A paused-worker test proves history does not mutate before acknowledgment. Synthetic egui clicks cover Texture Set List Add, inactive-set selection, rename and binding; layer-row selection now marks dirty and survives save/reopen and journal Restore; UV/model paint gestures include exact seam-safe dual-chart painting with atlas-gap and Undo/Redo assertions; mesh import Undo/Redo uses atomic journal transactions and tests restore both the imported and saved mesh states. Exports advance one PNG per UI frame with progress/cancel. AO baking runs on a cancellable worker and installs an editable Occlusion layer; pixels survive Save→reopen→Restore, and per-Texture-Set sample count, ray distance and seed now persist through project saves and recovery events.
- New integrated slices: S5-031 syncs the completed temporary project archive before replacement, syncs the parent directory on Unix, and reports post-replacement sync failure distinctly; app Save/Save As now keeps the committed project identity and journal transition. S8-004/005 provide standalone and editor material-ID export. S8-006 exposes high-to-low normal projection through a bounded CLI; S8-007 adds an optional source-coordinate affine transform and records its matrix and distance units. S1-013 adds deterministic OBJ/MTL and glTF/GLB mutations, rejects a malformed MTL panic input and catches a gltf-json parser panic as an import error. S1-014 imports, transforms, persists and renders glTF tangents in the CPU reference path, with UV-derived fallback. S1-016 rejects glTF skinning and morph targets with actionable errors. S1-017 preserves TEXCOORD_1–7 across import and mesh/project round trips, with UV0 remaining the active paint channel; UV8+ rejects. S6-004 verifies exported ORM/normal influence in Blender CPU Cycles. Windows replacement remains uncompiled locally; hosted Windows/macOS CI and desktop interaction remain unverified.
- Current integrated verification: `cargo fmt --all -- --check`, `cargo test --locked --offline --all-targets` (487 passed / 0 failed / 2 ignored), strict Clippy, `cargo check --locked --offline --all-targets --features gpu-experimental`, package, build and `scripts/qa/cli_release_smoke.sh target/debug/kalium` pass. The CLI smoke validates generic, Godot, Unity and glTF exports plus map outputs and source-project immutability. Hosted CI, cargo-deny and desktop GUI/GPU acceptance remain unverified.
- S15-002 configures Windows and macOS source-validation CI jobs (format, default and GPU-feature checks, tests, strict Clippy). Workflow structure and diff checks pass locally; hosted platform results remain unverified. Every roadmap stage S0–S15 remains IN_PROGRESS; the project is still an alpha prototype. Latest focused work is S1-017 and S5-032; desktop acceptance and broader stage-level gaps remain.
- Latest verified code commit: `9759b4f` (`test: verify ambient occlusion CLI output`); an isolated clean checkout passed all gates with 194 tests. Documentation checkpoint before this handoff edit: `1fb8891`.
- Verification checkpoint commit: `7883e51` (status and handoff update above the implementation commit).
- Historical starting state for this continuation: the branch contained accumulated implementation changes; `Cargo.lock` and `docs/roadmap/master-goal.md` were user-provided untracked inputs and are now tracked/retained.
- New documentation created in this session: `docs/architecture/overview.md`, `docs/roadmap/implementation-status.md`, `docs/roadmap/decisions.md`, `docs/roadmap/agent-workboard.md`, `docs/roadmap/session-handoff.md`, `docs/research/reference-evidence.md`, and `docs/qa/test-matrix.md`.
- Commit `12bc283` is the reviewed S0-003 slice containing only `.github/workflows/ci.yml`, `Cargo.toml`, and `Cargo.lock`. Commit `28a5555` integrates the reviewed source, tests, QA, and progress documentation.
- This continuation adds the manual checkpoint API/UI, CPU GGX reference and its numeric/pixel tests, the single-set and per-material Blender smoke scripts, and corresponding docs. The branch remains dirty and uncommitted; no files were staged.
- The current continuation adds `src/bake.rs`, a same-mesh `bake-mesh-maps` headless command, a shared-coordinate high-to-low projection primitive, a bounded `src/recovery.rs` journal framing module, regression tests and current S8/S13/QA audit findings. All changes remain in the shared dirty worktree; no files were staged or committed.
- This continuation integrated S5 image-event journaling, followed by selected layer structural events. Saves establish a UUID/digest-bound sidecar; committed image edits and supported layer property/order edits append bounded, synced batches; opening offers Restore/Discard; unsupported document changes create a disabled marker so partial recovery is not presented. Headless reopen tests cover image, layer-property/order replay and Discard. The 1-MiB raw patch-event budget and synchronous UI-thread sync remain limitations. Layer create/delete, set/mesh/binding, undo/redo events and desktop interaction remain open. A child-process kill/relaunch test now verifies a durable paint journal and torn-tail truncation. The suite then had 186 passing tests.
- Current dirty paths are tracked in `git status --short`; Cargo.lock and the workflow/manifest change are committed. The current implementation includes layer-row DnD, `Document::composite_region_for_set`, material-map region patching, partial UV texture updates for touched 64×64 paint tiles, and frame-stepped PNG export in `src/export.rs`.
- Isolated clone at `7883e51` is clean and passes `cargo fmt --all -- --check`, all 186 baseline tests, strict Clippy, GPU-experimental feature compilation, and `git diff --check`. This verifies the committed formatting/lint cleanup in `28a5555`. Remote CI remains unavailable because origin is a local bundle.
- The earlier source integration added AO CLI output and closed two recovery gaps: selected-layer replay is journaled, and an oversized committed patch disables recovery rather than exposing an incomplete older prefix. Regression tests cover both. The integrated suite was 191/191 at that commit. The `bake-ao` demo smoke writes a 512×512 RGBA PNG with 262,144 covered texels; AO output is not saved to projects.
- Commit `9fc6f91` integrates the AO CLI and recovery fixes. A fresh isolated clone at that commit passed formatting, 191 tests, strict Clippy, GPU-experimental feature compilation, diff checks and clean status. Commit `1316e71` adds app-level torn-tail recovery; its 192-test run was from the main worktree. Hosted CI remains unavailable because origin is a local bundle.

- Recovery follow-ups add app-level torn-tail prefix restore/append and complete checksum-corrupt frame rejection without a recovery prompt. Commit `9759b4f` adds an end-to-end AO CLI PNG test; its exact clean clone passed formatting, all 194 tests, strict Clippy, GPU-feature compilation, diff checks and clean status.
- S6-005 now has named Godot 4 ORM, Unity metallic-smoothness and generic glTF 2.0 metallic-roughness export profiles. They pin OpenGL (+Y), write per-set JSON sidecars, and are covered by exact packed-pixel/manifest tests plus rebuilt CLI artifact smoke. The glTF sidecar may point both metallicRoughnessTexture and occlusionTexture to the same ORM PNG (G=roughness, B=metallic, R=occlusion); this describes assignments and does not create a `.gltf` model. Docs and ADR D-034 record Khronos glTF 2.0 conventions. Remaining next step: advance to higher-priority unblocked work from S0–S15; desktop engine import/render acceptance is environment-blocked, but further headless slices remain available.

## Stage/task status

- S0–S15 remain `IN_PROGRESS` in the authoritative status table. Unit and headless integration checks pass for several slices, while full stage acceptance and desktop artist-workflow evidence remain incomplete. See implementation status for details.
- UI-001 is implemented and code-reviewed, with 2 policy tests; GUI interaction remains unverified.
- S5-001 and S1-001 image decode bounds are implemented and verified by regression tests.
- S1-002 headless TwoMaterials paint/save/reopen/export workflow is verified by pixel assertions.
- S1-002 now includes actual CPU viewport material-slot picks, hit-UV coverage checks, rerendered paint visibility and the existing per-set save/reopen/export pixel assertions.
- S2-001 analytic pixel→UV/material and overlap tests are verified in the full suite.
- S0-002 CI fmt/Clippy steps are added; S0-003 tracks Cargo.lock and uses `--locked` in commit `12bc283`. Latest clean-clone checks pass; remote CI is unavailable.
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
- S5-009/S5-010/S5-011 add bounded blank-layer creation, full-state layer duplication and compact selected-layer removal replay. App Add/Duplicate/Delete emits the corresponding event; active/inactive replay and invalid/no-mutation tests pass, as do app Save→Add/Duplicate/Remove→reopen→Restore.
- S5-012 adds bounded `TextureSetRename` replay for active and inactive sets, with trimmed 1–128-byte UTF-8 names and no content mutation. App Save→Rename→reopen→Restore passes with journal enabled.
- S5-013 adds bounded `TextureSetCreate` and `TextureSetRemove` replay. Create preflights the new blank mask's decoded-memory cost and preserves previous set state; remove selects the lowest-ID survivor and clears only bindings to the removed set. App create→paint and remove→reopen→Restore tests pass with journal enabled.
- S5-014 adds bounded material-slot binding and unbinding replay for the mesh saved in the baseline. At that milestone, mesh import and undo/redo edits remained unsupported; later S5-020 and S5-022 added bounded mesh replacement and mesh history recovery.
- S5-015 adds bounded active Texture Set selection replay by stable ID. App Save→select→reopen→Restore reactivates the selected set and keeps both sets' independent pixels, masks, layer IDs and resolution.
- S5-016 verifies model paint auto-selection is journaled before the stroke patch. The first recovery batch may include the corresponding layer selection event; app Save→pointer-paint→reopen→Restore recovers pixels on the bound Texture Set.
- A synthetic UV-pane pointer drag verifies hover-only no-op, committed stroke history, undo/redo, and Save→paint→reopen→Restore. A synthetic model-pane drag exercises the bound-set auto-switch path and journal ordering. Desktop GUI acceptance remains open.
- S5-017/S5-018 journal imported layers inside Begin/Commit markers around the create event, 256-KiB raw-byte full-width image patch rows, and final selection. A 1024² app test verifies exact channel pixels, name and selection across 16 patch records after Save→Import→reopen→Restore. The transaction scanner rejects malformed marker/body order and physically truncates an incomplete final import on reopen. A child process is killed after the create event and one row patch; a fresh process restores a prior committed paint and discards the incomplete import. A synthetic near-cap test verifies the disabled marker prevents any recovery prompt after append failure.
- S1-008 corrects triangle winding for negative-determinant glTF transforms and retains material-slot associations. Generated and supplied normal fixtures, UV/material picking, and singular-transform rejection pass.
- S15-005 adds `scripts/qa/cli_release_smoke.sh`, wired into CI after `cargo build`. It checks PNG signatures/dimensions, all 11 export files, bake metadata and source project hash unchanged; it does not test visual quality or engine import behavior.
- UI-004/UI-005/UI-006/UI-007 add synthetic egui interactions for Texture Set List Add, inactive-set selection, rename and material-slot binding; tests verify active identity/name and retention of independent layer IDs, pixels, masks and resolutions. Desktop behavior remains unverified; other set controls still need coverage.
- The checkpoint UI review confirmed the existing unsaved Cancel/Save transition paths and found the checkpoint is intentionally global across projects; the menu now labels it global, and saving is rejected if the checkpoint path is also the active primary project.
- S7-001 adds a deterministic CPU reference Cook–Torrance GGX shader with linear-light Base Color/Emissive and analytic numeric oracles. S7-003 adds bounded directional key and ambient-fill controls; it remains a CPU reference without IBL or GUI image acceptance.
- S7-002 adds a rendered-frame regression proving a committed Base Color map edit appears after the viewport rebuilds its material snapshot. This is a renderer regression, not end-to-end GUI acceptance.
- S8-001 adds a standalone same-mesh CPU bake command and `bake-high-to-low` with bounded OBJ/glTF/GLB inputs and staged normal/coverage/metadata publication. S8-007 later added optional source-coordinate affine alignment; there is still no correspondence cage. Direct-API parity, budget/distance rejection, rollback and source-project immutability are tested. Results are not saved in `.kalium`; additional maps and generator integration remain.
- S3-005 replaces full-image per-stroke snapshots with 64×64 touched-tile capture. A 4096² fixture verifies that one small dab captures less than 40 KiB of projected before/after data, repeated dabs capture each tile once, and undo/redo restore exact channel and mask bytes. Strokes exceeding the 96 MiB history budget roll back without clearing prior undo/redo. S13-006 recomposites unique touched regions for active paint and patches the cached map and UV texture; region output and patched cache bytes match full composites. Fill/clear and structural edits still use full invalidation, and viewport rendering remains full-frame CPU. These are code/test findings, not latency benchmark results; `perf`, heaptrack, valgrind and GUI profiling are unavailable here.
- S6-004 now covers Blender 5.2.2 single-set and multi-set import/node-link/save-reopen. Both the legacy single-set command and multi-set wrapper pass. The two-set CPU Cycles smoke checks distinct Base Color regions, exact ORM/normalized tangent-normal values, and Roughness/Metallic/Normal counterfactual render MAEs of 0.180954/0.143856/0.550776. This is a deterministic Blender fixture check, not broad engine parity or render-quality evidence.

## Verification run

- `cargo fmt --all -- --check` — PASS.
- `cargo test --locked --all-targets` — PASS, 256 passed / 0 failed / 1 ignored (manual Blender fixture generator) in the current worktree; commit `9759b4f` separately passed clean-clone fmt, all 194 tests, strict Clippy, GPU-experimental compilation, diff checks and clean status.
- `cargo clippy --locked --all-targets -- -D warnings` — PASS after stroke recovery patch extraction and replay validation.
- `cargo fmt --all -- --check` and `git diff --check` — PASS after manifest-identity integration.
- Earlier in this worktree, `cargo check --locked --offline --all-targets` and `cargo check --locked --all-targets --features gpu-experimental` passed (compile only).
- `cargo clippy --locked --all-targets -- -D warnings` — PASS.
- `cargo check --locked --offline --all-targets` — PASS.
- `cargo check --locked --all-targets --features gpu-experimental` — PASS (compile only).
- `cargo package --locked --offline --allow-dirty` — PASS, 58 files packaged and staged package compile passed; CI YAML parsed with PyYAML and the workflow invokes `cargo package --locked` on a clean hosted checkout.
- `cargo run --locked -- --version`, validate demo, preview demo, inspect brush, and generate UV mask — PASS; output artifacts were written under `/tmp`.
- `cargo run --locked -- export-all examples/Kalium-Demo.kalium /tmp/kalium-downstream` — PASS; exported 11 maps for the Default set.
- `cargo run --locked -- bake-mesh-maps examples/Kalium-Demo.kalium /tmp/kalium-bake-smoke-final` — PASS; emitted 512×512 world-normal, position and coverage PNGs plus JSON metadata for 262,144 covered texels; Python/Pillow verified file dimensions and metadata.
- `blender --background --factory-startup --python-exit-code 1 --python scripts/qa/blender_import_smoke.py -- /tmp/kalium-downstream/001_Default examples/TwoMaterials.obj /tmp/kalium-blender-smoke.blend` — PASS on Blender 5.2.2; verified PBR map load, Principled links, OBJ material slots, and saved `.blend` reopen. Blender logs read-only extension-cache cleanup warnings in this restricted environment; the smoke assertions pass and command exits 0.
- GUI launch was attempted using Wayland, X11 and Xvfb. `DISPLAY=:0` rejects `xdpyinfo`, `WAYLAND_DISPLAY=wayland-1` is unusable, sandbox permissions prevent Xvfb from binding `/tmp/.X11-unix`, `/dev/dri` is absent, and Vulkan has no working NVIDIA or software ICD. No GPU visual/performance test or Kalium GUI acceptance was run.
- The autosave architecture review found that a deep document clone would block and duplicate up to the decoded-document cap, while current mutations are too scattered to journal completely. A stable project UUID is persisted in manifest v6, Save As forks the UUID, and journal v2 headers validate UUID + SHA-256 baseline digest + schema. App journaling now covers committed channel/fill/mask image changes and has Restore/Discard recovery on reopen. Unsupported edits block replay via a sidecar marker. Structural events, background writer, GUI runtime evidence and crash-kill automation remain open; timed snapshot autosave remains disabled.
- Manual checkpoint review found no project identity or timestamp in the global checkpoint. The menu now calls it global, but the UI still does not show which project created it or ask for confirmation before replacing a clean current project. Repeated replacement behavior on Windows has not been verified.

## Risks / open issues

- The accumulated source is committed and passes the recorded checks; a complete line-by-line review of all existing implementation is outside this slice.
- Native save uses a sibling temp file and Open can recover a fully valid temp archive. Manual recovery checkpoints are available under the user config directory. The bounded journal is integrated for image edits with a Restore/Discard prompt, while broader mutation events, background syncing, desktop modal validation remains open.
- Texture resolution changes preflight the full decoded-byte budget, leave inactive sets untouched, and replay through the active-set resize event. Unsupported structural edits still disable the current journal until the user saves a new baseline.
- Fuzz coverage for decoder and project archive edge cases remains incomplete.
- Fill/clear history remains full-image. Active brush strokes patch touched map regions; fills, structural/property changes, and cache misses still rebuild full composites. Stroke latency and whole-app memory behavior have not been benchmarked.
- The composite output alpha remains opaque; `Kalium_BaseColor_Alpha.png` uses Base Color RGB and Opacity-channel red as its alpha, with a byte-level test. Emissive brush/layer blending, Normal vector stamps, Detail Normal RNM composition and scalar data-value blending are tested; downstream expectations remain open.
- CPU reference renderer now uses deterministic direct GGX Cook–Torrance shading and bounded key/ambient controls in Brush Studio. It is not native GPU PBR and has no IBL or GUI image acceptance. The glTF TRS bake fixture covers one transform case; broader transform edge cases, seam picking and geometry fuzzing still need tests.
- Validated temp recovery and image-edit journal recovery are integrated for supported events; other event families, desktop prompt validation remains open.
- No GUI-driven import → pick → paint → UV preview → save/reopen → export workflow yet.
- Blender 5.2.2 single-set and multi-set smokes verify exported map loading, Principled node links, `.blend` save/reopen, and persisted sRGB/Non-Color settings. The multi-set run also renders the reopened Principled materials and confirms the Roughness, Metallic and Normal maps affect the output against constant-input controls; broad visual quality and other engines remain unverified. Reproduce via `docs/qa/test-matrix.md` and `scripts/qa/blender_multiset_smoke.sh`.

## Next three concrete edits

1. Broaden glTF UV0–7 preservation coverage with external multi-UV fixtures and verify source-space/export behavior; painting still targets UV0. Skinning and morph targets remain fail-closed.
2. Implement a non-destructive layer-group model and compositor contract, including nested opacity, masks, channel enablement, reorder history and save/reopen tests. Freeze the tree schema before UI integration.
3. Resolve zip RUSTSEC advisories by obtaining patched zip >=9 source, migrating project archive APIs and removing the temporary exceptions. The current environment lacks that source offline; do not treat the scoped exceptions as a permanent fix.


The Blender multi-set fixture is generated by ignored test `project::tests::write_blender_two_set_fixture`; the orchestrator command is `scripts/qa/blender_multiset_smoke.sh target/debug/kalium`. It passes on Blender 5.2.2.

REA/Ghidra evidence: none used. Legal/source review was unnecessary for these ordinary engineering tasks.


## Latest continuation update

S5-025 integrates `JournalWriter` in the app. Ordinary batches enqueue without UI-thread disk sync; Undo/Redo holds document/history state until durable tickets resolve. Worker failure writes the disabled marker, while lifecycle replacement and app teardown drain and join the worker. Actor tests cover FIFO, item/byte pressure, dropped tickets, errors and drain; an app test holds the worker and proves Undo waits before changing pixels/history. Child-process kill tests cover incomplete async mesh and layer-snapshot transactions. S9-001 now attaches the bounded UV→seeded value-noise→Roughness graph to layers, composites roughness by scanline, persists graphs in manifest v8, exposes seed/scale controls, and integrates property history, recovery events and layer snapshot metadata. The latest integrated suite passes 377 tests with 2 ignored. Untitled-session baselines and saved-project journals now use OS advisory locks. Tests cover contention without journal/archive mutation, Save As target handoff, Open cancellation lock release, marker-failure invalidation, and pointer-driven Restore/Discard. Periodic full-project snapshots, hard-link alias coordination, cross-platform lock semantics, desktop recovery prompt and responsiveness remain open.

S9 graph edit→Undo→Redo→reopen→Restore passes. S1 now rejects missing UV0 with actionable importer errors, reports the invalid glTF primitive, and verifies failed path import leaves the document unchanged. S13 paint refresh now updates only visible screen samples mapped to touched texture tiles; all eight channel cases match a full-render framebuffer byte-for-byte, and the app partial-texture-refresh path is exercised in tests. Full suite: 400 passed, 0 failed, 2 ignored. The 1K/2K/4K release-mode viewport benchmark is now recorded; desktop visual/upload acceptance remains unavailable. S5 queue/memory responsiveness measurements and desktop recovery-prompt acceptance remain open. Desktop visual/GPU acceptance remains unavailable in this runtime. Mesh imports retain only position/normal/UV0; imports now reject missing UV0 rather than silently collapsing paint. Advanced brush workflows and UDIM support are absent.

S7-003 adds bounded azimuth, elevation, direct intensity and ambient-fill controls for the CPU reference shader. The previous default lighting is preserved. Renderer tests verify visible PBR changes, solo-channel byte invariance, non-finite/range handling and invalidation of the incremental-refresh cache after a lighting edit; a pointer-driven slider test confirms the app marks the viewport dirty. S8-003 stores per-set AO sample count, ray distance and seed in manifest extensions, journals edits by stable set ID, and restores settings with the project; pointer-driven sample/seed edits update metadata, and direct-bake comparison confirms the selected options drive the worker result. Recovery tests cover unknown nested options and malformed settings. The full suite passes 377 tests with 2 ignored; controlled PBR visuals and desktop bake progress/result acceptance remain open.
S11-001 now defines bounded 1001–1999 UDIM tile addresses and floor-based local UV mapping with positive-side integer-boundary ownership. Tile-specific CPU coverage clips cross-boundary triangles and independently filters material Texture Sets; tests confirm legacy local coverage equivalence. This is groundwork only: per-tile project/layer storage, painting, history, recovery, rendering and export remain unimplemented. Full suite now passes 377 tests with 2 ignored.


## Latest continuation — AO-driven Roughness integration

S9-002 adds an optional OcclusionInput and Multiply topology to the bounded procedural graph. The input resolves a stable layer ID within the current Texture Set and samples the stored AO red channel at matching texel coordinates; a missing layer/map is neutral white, so noise output remains. The Layers pane can create an AO-driven graph from an Occlusion layer, change the source, detach it, and shows a missing-source warning. Existing AO maps remain editable layer channels and continue to use project, Undo/Redo, and journal persistence. Manifest v8 stores the graph schema; v1–v7 load, and v7 files containing v8-only AO nodes are rejected. Full and bounded Roughness composites are pixel-equal; AO source tile edits patch dependent Roughness material-map regions. Targeted graph, project, app UI, cache, recovery and regression tests pass; the later integrated verification is recorded in the latest continuation below.

## Latest continuation — bounded mesh preflight and tile lookup

S1-005 now preflights both OBJ `f` and `l` records before `tobj` assembly and bounds MTL material declarations plus unknown parameter keys/values, including aggregate unknown-field storage across referenced libraries. A read-only parser review found the `l` record expansion and MTL unknown-map allocation bypasses; both now have limits and regressions. Import-level repeated-face and repeated-line fixtures confirm rejection before geometry assembly. Malformed syntax fuzzing and memory profiling remain open.

The child-process import-recovery regression exposed a test synchronization race: the parent treated the readiness file’s existence as a complete marker, so it could read the file between create and write. The parent now waits for the exact marker bytes before killing the writer. The isolated regression and integrated suite pass.

UI-009 is verified as a bounded first asset-shelf slice: the Assets pane searches two embedded `.kbrush` presets, applies their complete brush values, and displays an empty-result message; preset parsing and file loads cap inputs at 64 KiB. User-local indexing, tags, import and refresh remain open.

S13-007 prevalidates the full renderer change-rectangle list before modifying cached framebuffer pixels, then uses a bounded hash lookup for changed tile cells instead of scanning every changed rectangle for every screen hit. Tests compare a 64-tile update against a full render byte-for-byte and confirm an invalid trailing rectangle leaves the cached frame unchanged. S13-008 now refreshes direct-channel Fill/Clear results through 64-row strips in the existing clean material cache; it preserves a full 3D viewport redraw. Mask fills and Occlusion edits with dependent Roughness graphs deliberately retain full invalidation. Cache refresh failures no longer call the document-edit `change()` path, so they cannot disable recovery journals on their own.

## Latest continuation — local brush preset shelf

UI-010 integrates the bounded shelf scanner into the Assets pane. The shelf is stored beside the per-user workspace file in `brush-presets`; scanning is nonrecursive, ignores symlinks and unrelated files, sorts `.kbrush` paths, caps the directory at 512 entries and presets at 128, and reports invalid preset files. Search spans bundled and local presets. Import parses through the existing 64-KiB loader and writes a collision-safe copy with create-new semantics; refresh rescans the shelf. Synthetic egui coverage selects a local preset and compares the complete brush, while the import test confirms repeated imports do not overwrite.

Focused scanner tests pass (10 tests); the full suite passes 409 tests / 0 failures / 2 ignored, and strict Clippy passes. Desktop file-picker and visual acceptance remain unverified. Remaining S12 work includes tags, tutorial and accessibility/hidpi journey coverage.

## Latest continuation — importer and shelf follow-up

S1-006 closes a glTF material expansion gap: the importer rejects more than 256 material slots before constructing owned material-name strings. A 256-material glTF fixture imports with deterministic names; 257 is rejected. A read-only S1 audit also flagged repeated node-DAG traversal as remaining CPU-amplification risk, which still needs a global visit bound. S2/S3 audit identified sparse model-pointer movement that may skip intermediate seam triangles; current seam tests use dense adjacent pixel events, so sparse resampling remains unverified.

UI-010 local preset shelf remains integrated as above. Current suite: 409 passed / 0 failed / 2 ignored; strict Clippy, GPU-feature compilation, formatting and diff checks pass. The CLI release smoke and Blender 5.2.2 two-set import/render smoke both passed in this continuation. Desktop UI and graphics hardware remain unavailable.

## Latest continuation — glTF bounds and sparse model strokes

S1-011 caps glTF material slots at 256 before allocating the owned material-name vector and selected-scene node visits at 100,000 across the entire traversal. Tests preserve exact-boundary material order, reject 257 slots and reject a glTF scene that repeats a child beyond the node-visit cap. Malformed-input fuzzing and parser memory profiling remain open.

S2-006/S3-012 resamples a captured model-pointer segment through the CPU surface-hit buffer at at most one logical-pixel intervals (bounded to 2048 samples). It forwards triangle transitions plus the final endpoint, keeping same-triangle interpolation behavior and avoiding extra opacity dabs. A synthetic egui gesture skips directly across three UV charts, changes all three while leaving the atlas gap unchanged, and is one Undo/Redo item. All-target suite: 409 passed / 0 failed / 2 ignored; strict Clippy, default and GPU-feature checks, offline package verification, formatting and diff checks pass. The CLI release smoke and Blender 5.2.2 multi-set import/render smoke pass. Native desktop pointer/visual acceptance is still unavailable.

## Latest continuation — glTF parser bounds, editor shortcuts and reference environments

S1-012 wraps glTF document parsing in a hard `MAX_GLTF_FILE_BYTES + 1` reader after the metadata fast check and rejects parsed documents above 100,000 nodes, including unreachable nodes. Exact/over byte-boundary and unreachable-node tests pass. This constrains the parser source stream and document work but does not replace malformed-input fuzzing or peak-memory measurement; cargo-fuzz, heaptrack and valgrind are unavailable here.

UI-011 captures keyboard ownership before constructing menu widgets. Focused TextEdit fields retain Ctrl+Z, Ctrl+Y and Ctrl+Shift+Z for text editing; a headless egui test verifies history stays unchanged while focused and app Undo still runs without text focus.

S7-004 adds deterministic Studio/Warm/Cool analytic sky, horizon, ground and broad-source environments to the CPU reference shader, with AO-weighted diffuse and roughness-dependent reflected response. Brush Studio exposes the preset selector. Renderer tests cover image differences, repeatability, roughness response, invalid settings, raw solo maps and full-render/cache parity; headless UI selects Warm. This is an analytic CPU approximation, not HDR IBL or production rendering. Full suite: 416 passed / 0 failed / 2 ignored; strict Clippy and formatting pass. Desktop visual acceptance remains unavailable.

## Latest continuation — S8-005 material-ID editor export

The File menu and Texture Set Settings panel now request a PNG output path and launch the existing cancellable material-ID worker. The worker uses the active set's mesh, bindings, dimensions and identity snapshot; the editor and CLI share one metadata builder and staged PNG/JSON pair publisher. The UI shows an indeterminate spinner because the worker does not report row progress. Cancellation discards the result before writing; ordinary staging/publication errors restore a previous pair and leave the document/history untouched. Synthetic egui click, decoded PNG parity, sidecar, cancellation, error rollback and immutability tests pass. Integrated suite at that historical checkpoint: 422 passed / 0 failed / 2 ignored; formatting, strict Clippy and GPU-feature checks passed. Desktop visual acceptance, project-persisted baked assets and graph consumption remain open.

## Latest continuation — glTF tangent preservation and reference shading

S1-014 adds serde-defaulted optional tangent direction/handedness to imported mesh vertices. glTF tangents are transformed through nonuniform/reflected node transforms, checked against source normals, and carried through CPU raster clipping and perspective-correct interpolation into normal-map shading; missing tangents retain the UV-derived fallback. Project validation rejects malformed persisted tangent frames. Fixtures cover transformed import, invalid values/counts, old vertex JSON, `.kalium` save/reopen, tangent response, handedness response and fallback parity. Integrated gates at that checkpoint pass: 438 tests / 0 failed / 2 ignored, strict Clippy, formatting, GPU-feature compile, offline package (61 files) and CLI release smoke. Desktop/GPU tangent behavior and broader glTF corpus acceptance remain open.


## Latest continuation — workspace durability, OBJ preflight, and UV seams

UI-012 writes per-user workspace state via synced sibling temp files and atomic same-directory replacement; normal close flushes changes even within the 2-second autosave throttle. Unix syncs the parent directory. Headless tests cover replacement failure/success and a close immediately after layout changes. Windows native replacement remains uncompiled here.

S1-015 fixes OBJ preflight accounting for `tobj`'s one-/two-corner face expansion while keeping line-record corners under their separate bound. The many-short-face import regression rejects before parser assembly. Coverage-guided fuzzing and peak-memory profiling remain open.

S2-007 / S3-013 match duplicated mesh seam vertices within a per-triangle extent-scaled tolerance with one-to-one and ambiguity checks. The model-stroke integration test now includes slight positional drift; both UV charts change, the atlas gap stays unchanged, and Undo/Redo is exact. Focused seam coverage and the full suite pass: 453 passed, 0 failed, 2 ignored; the mesh module has 50 passing tests. Formatting and strict Clippy pass; desktop visual/input acceptance remains open.


## Latest continuation — UV detail navigation, resize peak limits, and dependency security

The UV viewport supports pointer-anchored wheel zoom (1×–16×), bounded right-drag panning, and double-click reset. Added headless egui integration coverage through the actual UV pane; it verifies the UV under the zoom anchor, pan translation, painting at the intended texel and exact save/reopen composites.

Document resize preflight uses actual-order transient allocation peaks rather than final storage alone. Added exact-limit, one-byte-over, downsize and inactive-set accounting tests; removed stale final-only helper functions. Full suite: 453 passed, 0 failed, 2 ignored. Formatting, strict Clippy and GPU-feature check pass.

Local cargo audit reports RUSTSEC-2026-0367/-0368/-0369 on direct dependency zip 0.6.6 plus unmaintained transitive paste and ttf-parser. Inspection shows project reads fixed archive entries via bounded ZipArchive::by_name and does not call extraction. Offline registry contains only zip 0.6.6; upgrade attempt to 9.0.0 cannot resolve. Added 30-day reasoned cargo-deny exceptions scoped to Kalium, and CI now runs advisory checks; the local cargo-deny binary and hosted CI are unavailable, so config integration remains unverified. Migration to patched zip source is still required; do not treat the exceptions as a permanent fix.

## Latest continuation — fail-closed glTF fidelity

S1-016 makes selected-scene glTF import fail closed for JOINTS/WEIGHTS, skin references, morph weights and morph targets. S1-017 preserves TEXCOORD_1–7 as per-vertex mesh data across primitives and project persistence; UV0 remains the active paint channel, and UV8+ rejects. Legacy mesh JSON defaults correctly. The full offline suite passes 464 tests / 0 failed / 2 ignored; formatting, strict Clippy, and GPU-feature compilation pass. Stages S0–S15 remain IN_PROGRESS. A live `cargo info zip@9.0.5` attempt failed at crates.io DNS resolution, so S15-008's temporary scoped advisory exceptions remain pending migration.

## Latest continuation — ZIP directory allocation preflight

S5-032 parses classic ZIP and ZIP64 directory metadata before `ZipArchive::new`, bounding entries to the project maximum, the full directory to 16 MiB, names to 256 bytes, and each extra field/comment to 4096 bytes. It validates central-directory record boundaries and caps ZIP64 footer scanning so the dependency cannot reserve attacker-declared tables or scan an unbounded prepended prefix. Tests cover exact/over entry limits, ZIP64 with a small prepended stub, oversized directories/names and load-level early rejection. The integrated suite passes 464 tests / 0 failed / 2 ignored; strict Clippy, formatting and diff checks pass. The dependency remains `zip 0.6.6`; advisory exceptions are temporary pending access to a patched release.


## Latest continuation — S4 recursive layer tree integration

S12-009 adds a versioned 256-entry user-local Activity log with bounded UTF-8 fields and a 256-KiB file cap. Writes use synced sibling temporaries, atomic replacement and Unix directory sync; failed persistence leaves the prior state intact. A dockable Activity pane is in the default workspace and View menu; headless egui coverage confirms persisted entries render, and a successful project save is recorded across reopen. Import, bake and layer/error event coverage plus native desktop acceptance remain open.

S4 has advanced beyond the original isolated tree model. `Document` and inactive Texture Sets now store recursive bounded Paint/Fill/Group trees; the compositor uses value plus coverage and tests nested group isolation and full/region parity. Manifest v9 stores trees and migrates v1–v8 flat layers. Stable-ID history covers tree property/fill/move/subtree changes; recovery streams subtrees in bounded transactions and tests interrupted snapshot truncation. All eight legacy versions now have a migration→save as v9→reload fixture preserving composites, masks, pixels, selection and extension data. A v9 save→load→save→load test preserves nested Group/Fill fields, extension payloads and composites. The Layers pane exposes Add Group/Fill, selection/reorder and fill values. The egui acceptance flow pointer-drags Paint into a Group and back out, edits Group opacity and a nested Fill, saves, reopens, then restores subsequent edits from the journal. A regression also prevents deleting the last Fill value. Manifest preflight now accounts for unknown extension data under one shared budget across root, Texture Set and tree scopes. Fixed the cross-parent move helper, which had computed but not used the destination parent, and closed a metadata-budget bypass for active-tree/root/set extensions. AO graph source selection/removal in the Layers pane also has a passing headless workflow test.

Verification after integration: `cargo test --locked --offline --all-targets` passes 521 / 0 failed / 2 ignored; strict Clippy, formatting and `cargo check --locked --offline --all-targets --features gpu-experimental` pass. S4-008 adds spatial Fill/Group mask strokes with sparse Undo/Redo, stable-ID recovery and composite cache invalidation; S7-005 adds deterministic full-frame CPU material fixtures. S4 remains IN_PROGRESS because broader nested-tree interactions and native desktop review remain; GUI/GPU hardware acceptance is unavailable in this environment.


## Latest continuation — S4 subtree duplication and recovery ordering

Implemented generic undoable duplication for Paint, Fill and Group nodes. Preflight rejects node/paint-leaf, retained-history, decoded-storage, extension-byte and ID-counter limits before mutation. Descendant IDs are rekeyed; AO source IDs are remapped only for references internal to the duplicated subtree. The Layers-pane duplicate button is exercised with synthetic pointer input for a populated Group and its nested Fill; two bounded transactions restore the nested content, masks and fresh IDs after reopen. A recovery reconstruction defect surfaced during this test: the forward builder consumed Groups before their descendants were attached. Reverse preorder attachment now builds children first and restores sibling order. Focused duplicate tests and the app Restore test pass.

Verification: `cargo test --locked --offline --all-targets` passes 529 / 0 failed / 2 ignored; strict Clippy, format check and `cargo check --locked --offline --all-targets --features gpu-experimental` pass. A separate focused test validates the actual duplicate-button click and crash Restore after the full-suite run. Root-level Fill duplication and native desktop interaction remain unverified; S0–S15 remain IN_PROGRESS.

## Latest continuation — selected-layer solo and roadmap follow-up

S4-011 adds transient selected-layer solo to the Layers pane and recursive preview composition. `LayerTree::composite_isolated` and its region variant retain the selected node's ancestor path and Group properties, while a selected Group keeps its full subtree. The active Texture Set's MaterialLibrary receives the isolated map for viewport/UV preview; stroke and uniform-edit region refreshes use the same solo-aware compositor. Toggling solo does not mutate document visibility or mark project content dirty. Focused tests cover nested Fill and Group results, ancestor opacity/mask/channel-disable semantics, full/region parity, unchanged visibility, pointer toggles, and byte-identical full-composite restoration.

The full offline suite then passed 531 tests / 0 failed / 2 ignored. Formatting, strict Clippy, and all-target GPU-feature compilation also passed after integration. Native desktop/GPU acceptance remains unavailable. The next independent high-priority work item from QA is bounded recovery journal checkpointing/compaction: current event journals cap at 256 MiB and append failure disables recovery. It touches the same app/recovery paths, so integrate it as a separate serialized slice after this one. All roadmap stages remain IN_PROGRESS.

## Latest continuation — bounded export resolution and recovery journal admission

S6-003 adds a per-export output-size choice from native Texture Set size or 64–4096 pixels. Maps are resampled before packing, with linear-light color filtering, vector-aware Normal filtering, data-map filtering, and UV coverage/padding evaluated at output dimensions. The export tests cover every output map, filtering behavior, output-resolution padding, unsupported sizes, and document immutability. D-076 records the contract.

S5-035 closes an append/scan limit mismatch: Journal append now rejects the first sequence beyond the scanner's one-million-record cap before writing. The regression verifies no bytes are added and the journal remains scannable. Safe generation-based journal checkpointing/compaction is still open and is the next major recovery task.

The full offline suite passes 536 / 0 failed / 2 ignored. Formatting, diff checks, strict Clippy, and all-target `gpu-experimental` compilation pass. Current roadmap position: all S0–S15 stages remain IN_PROGRESS; several individual headless slices are verified, with desktop interaction, platform/hardware gates, downstream engine rescaled export acceptance, and recovery compaction still open.


## Security verification finding

The current locked-graph read-only scan `cargo audit --no-fetch --deny warnings` exits 1 across 400 dependencies. It reports RUSTSEC-2026-0367/-0368/-0369 on `zip 0.6.6` (the last rated critical 9.1), and denied unmaintained warnings for `paste 1.0.15` and `ttf-parser 0.25.1`; the local advisory DB also reports an index-lock warning. Source review finds only bounded `ZipArchive::by_name` reads and no extraction call. `cargo-deny` is not installed, hosted CI is unavailable because origin is a local bundle, and offline cache contains only zip 0.6.6. Temporary advisory exceptions do not resolve the findings.


## Latest continuation — bounded viewport damage and recursive export fixture

S13-011 changes `Viewport::refresh_material_tiles` to return up to 64 disjoint screen-damage rectangles. Connected 16×16 screen cells are coalesced; allocation failure or more than 64 components returns `CacheInvalid` before framebuffer mutation. Renderer tests cover disjoint UV regions, exact frame parity, damage coverage, untouched-byte parity, adjacency coalescing and overflow atomicity. The app now accumulates bounded disjoint damage, merges overlaps and uploads each rectangle with `TextureHandle::set_partial`; more than 64 pending regions triggers a full render. The app-side accumulator cap/merge test passes.

Fixture 3 now builds a nested Group with black and white masks and a two-channel Fill, saves and reopens v9, checks exact composites and masks, exports before/after PNGs and asserts byte identity plus per-texel values. Current full offline suite: 529 passed / 0 failed / 2 ignored. Packaging, strict Clippy, formatting and GPU-feature compile are being checked for this integration. The renderer remains a CPU reference; native GPU viewport and desktop uploads remain unverified.


## Latest continuation — S5-036 generation-safe recovery compaction

- Date: 2026-10-19
- Branch/HEAD: `main` / `f3593d7` (unchanged; `.git` is read-only). The worktree still contains the shared pending S0–S15 implementation/docs changes; I made no staging or commit.
- Implemented: `src/recovery_store.rs` stores immutable project checkpoint generations with a digest-bound empty journal and atomically switches a synced manifest last. At 16 MiB, `src/app.rs` transfers the writer and real Document to a background checkpoint worker (no deep clone), which drains queued appends before snapshotting, and keeps pumping a progress UI while editing/file actions are paused; compaction starts only with no stroke/history/recovery/bake/export job active. Restore loads checkpoint then replays its journal. Discard writes a clean generation with no previous fallback. Save As changes generation identity. After a successful manifest switch, best-effort cleanup retains active and previous pairs and removes older/orphaned generation files; cleanup errors are non-fatal, surfaced as status warnings, and can leave stale files until a later publication.
- New regression coverage: Nine recovery-store tests cover active selection/high-water, corrupt active fallback, failed snapshot preservation, empty-journal append/reopen, failure after baseline/journal install and before pointer replacement, failure after pointer replacement, clean Discard without stale fallback, active/previous retention with orphan cleanup, and injected cleanup failure that reports a warning while preserving the published pair. The app integration test also verifies that a journal just below the injected threshold does not switch generations and that reaching the threshold does. App tests cover empty-journal checkpoint Restore and Discard, threshold boundary behavior, a gated worker progress frame, and Drop joining the worker while retaining the project lock until durable publication.
- Verification: `cargo test --locked --offline --all-targets` → **549 passed, 0 failed, 2 ignored**; `cargo clippy --locked --offline --all-targets -- -D warnings` PASS; `cargo fmt --all -- --check` PASS; `git diff --check` PASS. The same verification also passed `cargo check --locked --offline --all-targets --features gpu-experimental`; Windows/macOS compilation and crash acceptance remain unavailable here.
- Updated README, compatibility, implementation status, agent workboard, decisions (D-077), release notes and QA matrix. All stages S0–S15 remain IN_PROGRESS.
- Risks/open acceptance: cleanup warnings are surfaced but failed removals may persist until the next publication; the worker drains accepted appends before snapshotting and editing pauses during serialization. Native desktop Restore/Discard and Windows/macOS/device-level crash durability remain unverified. Full-disk behavior, worker failure/restart behavior, and large-project checkpoint latency need explicit tests/measurements. A read-only audit rejected deep cloning because it could duplicate up to 512 MiB of decoded images; the implemented worker instead owns the real Document during publication.
- Next edits: add app-level worker failure/restart tests; measure writer-drain and serialization latency on representative projects; then pursue desktop and cross-platform crash acceptance. If editing is later allowed during snapshot work, add a journal-fenced tail replay protocol before switching to an empty generation.

## Latest continuation — S5-036 close handling and crash-left staging cleanup

- Revalidated the task attachment, required project docs, roadmap status and dirty worktree. Branch/HEAD remain `main` / `f3593d7`; shared pending S0–S15 work is preserved and no files were staged or committed.
- Extended `recovery_compaction_worker_keeps_ui_pumping_without_cloning_document` to inject an egui root close event while the checkpoint worker is gated. The progress path emits `CancelClose` and the worker remains active. Focused test passes.
- `src/recovery_store.rs` now removes only exact `snapshot-<valid UUID>.tmp` and `manifest-<valid UUID>.tmp` regular-file names after durable publication. These are files a killed process can leave behind before RAII cleanup runs. The generation retention test verifies both are removed while an unrelated `.tmp` file is preserved. All 9 recovery-store tests pass.
- Verification at that checkpoint: `cargo test --locked --offline --all-targets` → **550 passed, 0 failed, 2 ignored** (552 tests discovered); focused worker-close and injected failure/restart Restore tests pass; recovery-store tests pass 9/9. `cargo fmt --all -- --check`, `git diff --check`, strict Clippy and GPU-feature compilation passed at that checkpoint.
- Updated the S5-036 workboard, S5 implementation status, QA matrix and release count from the captured full-suite result. All roadmap stages S0–S15 remain `IN_PROGRESS`.
- Remaining highest-priority S5 work: more worker failure/restart variants and representative project checkpoint/drain latency measurements. S13 still scans cached surface hits twice during incremental refresh, and transparent compositing is bounded to eight surfaces. Release blockers include the unresolved `zip 0.6.6` RustSec advisories and unavailable hosted/platform/desktop acceptance.

## Latest continuation — artist viewport controls and Painter API preset handoff

- Added Layer Group collapse/expand UI state and headless pointer coverage; collapsing does not mutate project content or lose stable selection.
- Added 3D viewport toolbar actions for camera reset and Studio/Warm/Cool preview environment selection. Headless egui test verifies camera reset and environment changes invalidate the rendered view.
- Added Painter export profile in the UI and CLI. It exports standard PBR PNGs plus `Kalium_SubstancePainter_ExportPreset.json`, a generated `name`/`maps` payload using the publicly documented Painter Python export API. This is not `.spexp` shelf serialization or `.spp` import/save. Painter runtime validation is pending.
- `cargo test --locked --offline --all-targets` passes **553 tests, 0 failed, 2 ignored** (555 discovered), including the new UI and export payload tests. Focused tests pass. `cargo fmt --all -- --check`, `git diff --check`, strict Clippy, and all-target `gpu-experimental` compilation pass.
- The user requested a runnable app copy earlier. The native window could not launch in this environment because there is no Wayland compositor and the available X11/Xvfb sockets cannot be opened. Run from `/home/DrapNard/Dev/Kalium` with `cargo run --release`; hardware GPU and desktop visual acceptance remain open.
- Next: implement the native WGPU mesh preview with the existing CPU rasterizer retained as the reference/fallback; use a Painter installation to validate the documented API payload before broadening Substance-format claims. Direct `.spp` import and save remain unsupported.


## 2026-10-19 continuation — experimental GPU viewport and UI status

- Added an optional WGPU 3D preview pane path: offscreen color plus depth targets, camera uniform, mesh vertex/index buffers grouped by material-to-Texture-Set binding, and each group samples its bound Base Color composite with simple diffuse light. The experimental preview toggle is labeled in the viewport toolbar. CPU PBR rendering remains the reference for picking, painting and the richer shading controls. This is not full GPU PBR, GPU picking or GPU painting.
- Added hardware-free tests for WGSL parsing/validation, camera finiteness/orbit/zoom/aspect, material routing, UV packing, and malformed geometry limits. Focused tests pass 4/4. Full suite: 557 passed, 0 failed, 2 ignored. `cargo fmt --all -- --check`, strict Clippy, `cargo check --locked --offline --all-targets --features gpu-experimental`, `git diff --check`, and debug build pass.
- App is available from `/home/DrapNard/Dev/Kalium`; run with `cargo run` or launch `target/debug/kalium`. A process was started in this session, but desktop visuals and actual GPU rendering remain unverified. GPU render hardware may be present, but no desktop capture/interaction was available.
- `.spp` remains signature detection only. The Painter export helper writes ordinary texture maps and the documented Python export preset JSON payload; it does not read/write `.spp` or an `.spexp` shelf file.


## 2026-10-19 UI clone refinement

- The UI direction is now a closer structural/style match to the default Substance Painter workspace: dedicated Assets shelf, side-by-side 3D/UV views, stacked Texture Set List/Layers/Properties, contextual Brush Studio/settings tabs, compact dark panel theme, File/Edit/Mode/Window/Viewport menus, and a global brush/channel toolbar. No Painter logos, proprietary icons, sample art or other copied assets were added. The user clarified that they want a close clone; the exact screenshot/reference they have in mind is not present in this repo, so the current pass follows the standard Painter workspace.
- Added a topology test for the default dock layout. Full suite: 558 passed, 0 failed, 2 ignored. Formatting, strict Clippy, `gpu-experimental` compile, debug build, and diff check pass. The earlier launch reused a stale executable. After `cargo clean -p kalium`, a fresh debug binary was rebuilt and launched with `XDG_CONFIG_HOME=/tmp/kalium-painter-clone-v2`. Its newly persisted workspace confirms the Assets pane is alone in its own column and the new panel splits are loaded; the user's saved workspace remains untouched.


## Latest continuation — paint allocation bounds and Xvfb runtime evidence

- Date: 2026-10-19. Branch/HEAD remain `main` / `f3593d7`; the worktree contains shared pending implementation and documentation changes. No files were staged or committed.
- `Document::ensure_selected_channel_capacity` checks lazy RGBA channel allocations against `MAX_DECODED_BYTES` before paint mutation. No-op opacity/erase/transparent color stamps and empty brush-tip coverage do not allocate. Over-budget strokes stop before mutation. Symmetry no longer applies a second partial-opacity dab on the centerline. Added budget-boundary and paint regressions.
- Corrected a near-plane picking test oracle to use the rasterizer’s existing 1e-5 barycentric edge tolerance.
- Verification: `cargo fmt --all -- --check`, `cargo test --locked --offline --all-targets` (**562 passed, 0 failed, 2 ignored**), `cargo clippy --locked --offline --all-targets -- -D warnings`, and `cargo check --locked --offline --all-targets --features gpu-experimental` pass.
- Runtime: Xvfb `:99`, 1920×1080, `WGPU_BACKEND=vulkan`, NVIDIA RTX 3060. Kalium opened with experimental Base Color preview visible. A pointer-driven 3D stroke changed the cube; Edit→Undo restored the sampled pixel after journal acknowledgment. Images are under `/tmp/kalium-xvfb-*.png`. File→Save As did not open a dialog; recovery prompt, remaining pane workflows, performance and cross-platform UI acceptance remain open.
- Updated roadmap and QA counts/evidence. S0–S15 remain IN_PROGRESS. Preserve the current original Kalium visual direction and use reference images for structural/interactions acceptance; do not pursue proprietary branding/assets or claim a complete visual clone.


## 2026-10-19 continuation — fixed-size Painter-style visual pass

- Re-read the supplied exact-visual-style brief and repository guidance. Opened all five reference images at full resolution. No Adobe marks, proprietary icons, preset files or image-generation tools were used.
- Updated default dock proportions to match the supplied split 3D/UV screenshot: 50 px tool rail, 374 px Assets, about 580 px per viewport, and a 328 px right stack at 1920×1080. The right stack is split into Texture Set List, Layers and Properties in matching proportions. Refined the selected row to a dark surface with a narrow blue edge, made Texture Set entries compact, replaced text-heavy layer actions with monochrome glyph controls, shortened channel labels, centered the two bundled brush previews, and paired the Properties previews.
- Captured `docs/visual/kalium-painter-1920x1080.png`, close-ups under `docs/visual/captures/`, and an open File-menu capture. The image uses Xvfb `:99` with Vulkan and an RTX 3060. Xvfb has no window manager, so the app screenshot lacks native titlebar chrome; this is recorded in `docs/visual/painter-classic-status.md`.
- Verification after the final UI changes: `cargo fmt --all -- --check`, `git diff --check`, `cargo build --locked --offline`, and `cargo test --locked --offline --all-targets` (**570 passed, 0 failed, 2 ignored**). One test initially caught the old dock ratio expectation; that assertion was updated to the new default geometry and the full suite then passed.
- Remaining screenshot differences: only two real bundled brushes and no material/alpha shelf, default project contains one Paint layer and a simple cube, some glyph silhouettes differ, native window chrome is not captured here, and hover/focus/disabled/drag/tooltip/scrollbar/modal states remain unreviewed. This is not a visual-completion claim. S0–S15 remain IN_PROGRESS.

## 2026-10-19 continuation — compact grid and native icons

- Reopened the five public reference screenshots at their original dimensions and opened the official Painter pages for Interface, Toolbars, Assets layout, Layer Stack and Viewport. Captures stay outside the repository; no Adobe imagery or icon assets were copied.
- Made the built-in and user-local brush previews use a responsive compact grid, replaced missing-font Unicode tool/layer action symbols with native vector glyphs, tuned the default/reset camera framing slightly closer, and changed the fallback viewport size to the 1920×1080 capture target. These changes are visual and preserve existing actions.
- Clean-built Kalium after Cargo initially reused a stale executable, relaunched it under Xvfb `:99`, captured the 1920×1080 client area and refreshed full-window, comparison, panel and File-menu screenshots under `docs/visual/`.
- `cargo fmt --all`, `cargo build --locked --offline` and `git diff --check` pass after the changes. Tests were not rerun. See `docs/visual/painter-classic-status.md` for the screenshot findings and remaining gaps; S12-010 remains IN_PROGRESS.

## 2026-10-19 continuation — Painter comparison corrections

- Re-opened the five reference screenshots from the supplied brief at their original resolutions and compared the actual Kalium 1920×1080 capture region by region.
- Tuned first-launch right-stack proportions to the 2026 Painter reference and added a sampled grayscale mask thumbnail with a narrow channel marker for layers that actually have masks. Mask pixels are only read for display; document data and painting behavior are unchanged.
- Refreshed the full-window, panel-crop and open File-menu captures in `docs/visual/`; updated `painter-classic-status.md` and S12-010 with the remaining asset-shelf, sample-project, window-chrome and interaction-state differences.
- `cargo fmt --all`, `cargo build --locked --offline`, and `git diff --check` pass. A fresh Xvfb launch captured the actual 1920×1080 UI. Full tests were not rerun; visual acceptance remains IN_PROGRESS.

## 2026-10-19 continuation — Open Recent and current QA evidence

- Re-read the active attachment goal and current repository guidance. Previous
  turn classified as progress: visual code changes plus inspected runtime
  captures. All S0–S15 remain IN_PROGRESS; the full master goal is not achieved.
- Integrated S12-011/P0-A under the written `recent-projects-contract.md`.
  Storage worker owned only `src/recent_projects.rs`; parent integrated module,
  per-user path, initialization, menu and lifecycle tests. Independent review
  found no concrete app lifecycle defect; parent added failure/race coverage.
- Recent history is bounded to 12 canonical paths / 4096 UTF-8 bytes per path /
  32 KiB, preserves supported-version extensions and persists transactionally.
  File → Open Recent uses the normal lock/recovery/unsaved-choice flow. Missing
  paths are disabled/removable; recovery sources and canceled/failed opens stay
  out. Recent-history write failure cannot turn a successful save into failure.
- Full suite: **590 passed, 0 failed, 2 ignored**, with two test threads. It
  exposed an older visual-pass expectation of camera distance 5.4; the reset
  pointer test now checks return to the initial camera framing (currently 4.9).
  Final Recent row alignment/disabled-tooltip polish passes all 19 focused tests.
- Actual 1920×1080 Xvfb acceptance opens a temporary copy of the real Kalium
  demo through Recent; inspected textured model/layer and the persistent Opened
  activity entry. New captures are under `docs/visual/captures/open-recent-*`.
  This is a narrow virtual-display slice, not full visual or desktop acceptance.
- Revalidated local cargo-deny 0.20.2: offline locked all-feature license and
  advisory checks pass; license scan warns only of unused MIT-0/MPL-2.0 entries.
  Updated current stale scan claims; hosted results remain unavailable.
- Preexisting mixed dirty changes are preserved; no integration commit created
  because app integration depends on that uncommitted baseline. Next priorities:
  pane restoration/persistence interactions and remaining visual-state captures;
  GPU region-update parity, native dialogs/recovery, hosted platform checks,
  baked-map storage and larger roadmap functionality remain open.

## 2026-10-19 continuation — legacy identity and rejected recovery preservation

- The prior status-only goal turn was no progress; resumed implementation from
  the live worktree and reviewed the two reported recovery preservation defects.
- S5-038 derives stable legacy archive identities from the existing open file,
  preserves v6+ IDs, and migrates the derived ID through normal v9 Save.
- Rejected identity/generation data is preserved through Save, Save As, Open and
  Discard; protected autosave data cannot replace a fresh document. Save sequence
  scan/count/overflow and failed Restore/baseline reload paths now protect their
  source before disabling recovery. Unsupported-edit invalidation is unchanged.
- Old random-ID legacy journals cannot be automatically recovered by this fix;
  they remain rejected and preserved. A different Save As target starts fresh
  recovery. No claim of recovering those old journals is made.
- Final full suite: **601 passed, 0 failed, 2 ignored**. Strict Clippy and
  all-target GPU-feature check pass after all source fixes; formatting, build,
  source package verification and diff check pass. The final full suite includes
  the last failed-Discard regression. Package verification reported 82 files at
  its checkpoint, before the remaining evidence/documentation updates.
- Fresh Xvfb :98, 1920×1080, isolated config under
  `/tmp/kalium-legacy-acceptance-5ita8shr`: opened legacy demo twice through
  Recent with no identity warning; both opens recorded in Activity. Painted a
  pink dab, waited for journal acknowledgment, SIGKILLed only the test process,
  relaunched, opened through Recent and clicked Restore. The sampled model and
  UV dab regions match pre-crash pixels exactly; original archive remains
  byte-identical. Captures: `docs/visual/captures/legacy-recovery-*.png`.
- This is narrow virtual-display acceptance, not full desktop/platform or
  Painter visual completion. All stages remain IN_PROGRESS. Next work returns
  to workspace/pane interactions and screenshot state fidelity.

## 2026-10-19 continuation — workspace and modal interaction corrections

- Independent UI audit identified empty workspace reload fallback and global
  Save shortcuts running behind the unsaved-choice modal. No project-content
  loss path was found in pane closing; valid old layouts intentionally retain
  user proportions until Window → Restore default workspace.
- Layout worker owned only `src/layout.rs`; parent reviewed the bounded loader,
  optional schema marker/legacy compatibility and empty-dock restoration tests.
  Parent owns `src/app.rs`: explicit keyboard dispatch now returns while an
  unsaved-change or recovery modal is pending. Added synthetic key regression
  for Save, Save As, Open and tool-mode shortcuts behind an unsaved choice.
- Final full suite passes **604 tests, 0 failed, 2 ignored**. Focused modal
  regression passes. Formatting, strict Clippy, all-target GPU-feature
  compilation, debug build and diff check all pass. Source package verification
  passed at the prior recovery checkpoint; it was not repeated after these
  workspace changes.
- Next UI work: compare toolbar control density and pane interaction states
  against full-resolution references; capture decorated desktop where available.
  Keep real asset library limits visible; no fabricated presets or Adobe assets.

- Next concrete visual task from bounded toolbar review: replace the four
  embedded-label sliders with compact thin-track/narrow-numeric-field/separate
  label groups. Keep exact existing brush bindings/ranges, avoid feature changes,
  and compare actual 1920×1080 runtime captures to the full-resolution references.

## 2026-10-19 continuation — canonical Painter visual composition

- Prior goal turn was progress: implemented recovery/workspace fixes, ran tests,
  and collected actual crash-recovery evidence. Continued toward the full goal.
- Reopened all five brief references at original resolution. Corrected the
  earlier visual target: the supplied Kalium split screenshot is a baseline;
  the brief's modern Painter 2026 image is now the canonical default composition.
- Parent owns app/theme integration; worker owned layout topology/focus helper
  and tests. D-082 records the shared display/control contract. Narrow rails and
  Assets, dominant 3D canvas with UV tab, and right inspector stack replace the
  first-launch split; saved layouts are preserved and Window → Restore default
  workspace applies the new preset to an existing installation.
- Added compact label/value-over-track brush controls, viewport-owned stripe,
  right pane shortcuts, larger square layer previews, separate blend/opacity,
  contextual Properties tab title, near-square paired previews, curved original
  eye/stroke glyphs and full transparency checkers. No Adobe assets or generative
  image tools were used.
- UI display fills the dock while keeping CPU/GPU buffers within existing caps
  and with matching aspect. Added wide-dock pointer-paint regression; added
  toolbar numeric/pointer tests and pane shortcut restoration tests.
- Runtime initially exposed a stale debug executable despite Cargo reporting
  Fresh. A package clean/build produced a new timestamp and the actual new
  layout; do not use pre-clean captures as acceptance evidence. Final capture
  and integrated gate results are recorded below when complete.

- Final UI/publication checkpoint: **614 passed, 0 failed, 2 ignored**, strict
  Clippy, GPU-feature compilation, formatting and diff check pass. Actual current
  capture is `docs/visual/kalium-painter-1920x1080.png`, with refreshed panel crops,
  File menu, numeric-edit and dock-shortcut states. A reference/app comparison
  stays outside the repo at `/tmp/kalium-painter-2026-comparison.png`.
- Live Size field was edited to 64 and restored to 22; Activity shortcut selected
  the real Activity body, then Properties was restored. Contextual Properties
  titles now use a stable Pane-based egui ID. Visual differences remain explicit.
- Latest owner steering: publish everything to a **public** GitHub repo, and
  refocus on **real editable SPP open/save as the sole target project format**.
  Further UI polish is deferred. Read `spp-focus.md`; `.kalium` alpha persistence
  and detection-only SPP inspection do not satisfy that requirement. Native Save
  now rejects SPP paths before mutation, with a preservation regression.
- Existing origin is a local git bundle. GitHub account DrapNard is authenticated;
  no matching Kalium repository was found in its accessible repository inventory.
  Owner confirmed public creation after the target question. Publication target:
  DrapNard/Kalium; actual URL/commit verification will be recorded after push.
- REA Mac connected, no target open initially, Ghidra 12.1.4 available. Bounded
  application-name discovery found the installed Painter app. No SPP fixtures
  found in the bounded local Downloads/Dev search. Next: controlled synthetic
  Painter fixture generation, public API observation and format contract. No
  proprietary sources/shaders/resources or SPP files are to be published.

## Public GitHub publication

- Created **public** `https://github.com/DrapNard/Kalium` with the owner’s explicit
  direction. Integration commit `2cb60c9c73eff775ca71b97ba41e1ca81e804919`
  includes the complete pending source/docs/visual work. Main and all ten existing
  tags were pushed. Remote `github` tracks main; original local bundle `origin`
  remains intact. `git ls-remote` matched the integration SHA and GitHub reported
  PUBLIC visibility/default main.
- Hosted CI run `39278200413` exists; Linux, Windows and macOS jobs are queued,
  not passed. Local 614-test/lint/GPU-check results remain the verified gates.
- Current application binary was rebuilt after a package clean; source remains
  an alpha. SPP import/write remains unsupported despite being the new priority.
- Connected Mac Painter metadata is **11.1.3**, ARM64 executable SHA-256
  `43c14c7a679c5bde792460dca975a801e8fd2e1978d06a4e2fb0eb987353bf08`.
  This is installed-reference identity, not format compatibility evidence.

## SPP fixture checkpoint

- Added original `scripts/research/painter_spp_fixture.py`; actually ran it on
  owned Painter 11.1.3, API 0.3.5. Full baseline and copy save; baseline reopens
  editable with unchanged digest. This is oracle research, not Kalium support.
- Private corpus: `/tmp/kalium-spp-corpus-fkci5p3l`; remote original directory
  `/var/folders/6k/y3vsv6l15fv_jxpmjh2kxh9c0000gn/T/kalium-spp-fixture-_6d2k_28`.
  Never publish its SPPs/resources. See `docs/research/spp-roundtrip.md` for hashes,
  public contracts, observed HDF5/document metadata and unverified hypotheses.
- Independent audits confirm the document schema is not decoded and mesh format
  1.7 differs from upstream ASSBIN 1.0. Do not use stock Assimp as assumed support.
- Next: controlled semantic differential fixtures, document/hash/mesh contracts,
  then bounded native import and genuine edit/save verified in Painter.
- No Rust behavior changes in this checkpoint; previous 614-test gate applies
  to the published application revision. Python harness has actual runtime evidence.

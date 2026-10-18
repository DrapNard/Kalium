# KALIUM — MASTER GOAL / AUTONOMOUS ENGINEERING PROGRAM

**Owner:** DrapNard / Vulpyr
**Execution agent:** Codex Luna High
**Source repository:** `~/Dev/Kalium` on CachyOS/Linux
**Research agent interface:** `rea-mac` MCP, pointing to a MacBook Air M2
**Research stack:** REA 6.7.0 + Ghidra 12.1.4 (native ARM64), Adobe Substance 3D Painter as an *authorized behavioral reference only*
**License target:** AGPL (confirm existing repository license and honor its exact version)
**Product principle:** NO GENERATIVE AI in the shipped application. Using Codex to *develop* Kalium is allowed; Kalium itself must not require AI, APIs, model downloads, online accounts or cloud inference.

---

## 0. YOUR MISSION AND EXECUTION CONTRACT

You are the principal engineer responsible for **completing Kalium as a usable, performant, maintainable, free/open-source 3D PBR texture painting application**, inspired by the *workflows and level of capability* of Substance 3D Painter, not by its proprietary source code, shaders, designs or assets.

**Do actual engineering work, not only an architecture document.** Inspect the repository, write code, run tests, integrate UI flows, diagnose failures, and deliver functioning features in an incremental sequence. Keep working through the roadmap while the current session has scope and resources. If the program exceeds a single session, leave a precise, machine-readable handoff enabling the next session to continue without repeating investigation. Do not claim work is complete merely because a plan, skeleton, placeholder or mocked feature exists.

**Global success state:** a user can import their own 3D asset, paint and procedurally author physically based textures in real time, work with non-destructive layers and masks, bake mesh-derived maps, save and recover a native project, and export validated textures for Blender, Godot and Unity—all using an accessible desktop GUI. Implement advanced capabilities after the end-to-end workflow is reliable.

**Rules of autonomy:**
1. Do not stop at a planning report. After a bounded audit and plan, begin implementing immediately.
2. Proceed independently within the existing repository and ordinary development tasks. Do not repeatedly ask the user to choose between equivalent implementation details; use sound defaults and record decisions.
3. Ask for user input only when materially blocked (e.g. credentials, permission, legally restricted analysis, unavailable external proprietary tests, truly destructive system changes, or fundamentally contradictory product requirements). Never circumvent a blocker.
4. Do not alter unrelated working code, delete user data, reset branches, force push, rewrite shared history or run uncontrolled destructive commands. Preserve any preexisting user changes.
5. Use short, reviewable commits on a feature branch when the Git state permits. Record the hash and tests for each delivered slice. Never commit credentials, binary dumps, proprietary files or huge generated build artifacts.
6. Respect existing project choices. First determine the actual framework, cargo workspace, UI, GPU and persistence architecture. Adapt this document to the repository; avoid a vanity rewrite or switching egui/wgpu unless evidence establishes a significant need.
7. Use strict **vertical slices** and demonstrable end-user behavior. A feature is not delivered if it is inaccessible in the GUI, not persisted when necessary, untested, or nonfunctional on actual content.
8. Favor correct data flows, debuggability, stability and recoverability over adding dozens of buttons.
9. Optimize after profiling, but build the architecture to allow GPU acceleration, incremental evaluation, large textures and cross-platform backends.
10. At each phase boundary, update the persistent progress files described below; then continue with the next unblocked dependency.

### Highest-priority instruction

**Keep Kalium working at all times.** Never sacrifice existing brush behavior, UV data, per-material texture assignment, mesh integrity, document persistence or UI workspace state just to introduce a new feature. Establish regression tests before modifying those paths.

---

## 0A. MULTI-AGENT EXECUTION CONTRACT — REQUIRED

**Execution model:** Run the program as a persistent Codex `/goal` with a principal/coordinator (Luna High) and up to **four concurrent subagents**. This is a *concurrency ceiling*, not a demand to spawn four agents during every task. A subagent is useful when there is an independent, bounded deliverable; dependent work stays sequential. Never fake agent execution if the environment does not expose subagent tools.

### Main agent: technical director and integrator

The main agent owns:
- Product-level definition of done, task/dependency graph, architectural contracts and acceptance criteria.
- Initial repository audit synthesis; scope negotiation; task allocation; all merges and integration decisions.
- Repository-wide migrations, cross-crate public API changes, global event systems and schema compatibility.
- Global CI, release checks, final GUI verification, performance baselines and delivery reports.
- Resolving incompatible proposals and reconciling changes from different subagents.
- Tracking whether every claimed outcome is **VERIFIED**, **IMPLEMENTED/UNVERIFIED**, **BLOCKED**, or **NOT STARTED**.

Subagents must not independently change the architecture or shared contracts in ways that surprise other agents. The main agent can delegate research or implementation of a contract but retains approval before downstream coding.

### Preferred subagent roles (assign dynamically; use available built-ins or custom agents)

| Role | Primary domain | Example bounded deliverable | Model effort guidance |
| --- | --- | --- | --- |
| `paint-core` (worker) | Brush engine, stroke sampling, UV/mesh hits, channel compositing, layers and masks | Deterministic stroke-spacing algorithm plus tests in owned module | High for difficult geometry, medium for small changes |
| `gpu-render` (worker) | GPU textures, material previews, shader integration, incremental rendering, baking | Offscreen PBR fixture with deterministic reference checks | High for architecture, medium for defined implementation |
| `ui-workflow` (worker) | egui/native UI, tool state, shortcuts, accessibility, workspaces | Working editing interaction for an already-defined paint API | Medium or High depending on complexity |
| `research-qa` (explorer/reviewer) | Public standards, REA inventory if authorized, test fixtures, regression and performance analysis | Evidence-backed behavioral spec with acceptance tests | Medium for inventory, High for difficult reviews |

**Role names are descriptive, not prerequisites**: Codex can use built-in `worker` and `explorer` agents if custom roles are not configured. Do not create an entire agent framework before developing functionality.

### First wave: audit in parallel, read-only

After obtaining the repository status and before changing code, launch up to four bounded, **read-only** tasks:

1. **Core audit:** current document/mesh/material/brush/layer/history behavior, existing tests, probable correctness defects.
2. **Graphics audit:** GPU path, viewport, texture update flow, import pipeline, memory/performance risks, supported platforms.
3. **UI/workflow audit:** existing editor navigation, essential artist flows, missing integrations, workspace serialization.
4. **QA/research audit:** public specifications, test coverage gaps, baseline commands, existing documented constraints; check remote `rea-mac` availability only if helpful. Do **not** begin proprietary decompilation without verified authorization.

Each reports: inspected paths, confirmed behavior, missing contracts, risks, concrete testable next tasks, and evidence. The parent consolidates into `docs/architecture/overview.md`, `docs/roadmap/implementation-status.md`, and the initial DAG, then assigns the first implementation wave.

### Every implementation wave

1. **Select the critical path:** map candidate tasks to S0–S15, identify prerequisites, and choose up to four genuinely independent tasks. If fewer exist, use fewer agents; if a task blocks the next, finish it before parallelizing downstream work.
2. **Freeze interfaces first:** main agent defines the minimal shared API, data types, error types, serialization policy and test expectations. Publish the contract in the repo. All subagents must read the same version before editing.
3. **Write an assignment contract** for each subagent with: task ID, goal, owned files/directories, read-only dependencies, interfaces to use, files expressly forbidden to modify, required tests, performance budget if applicable, output/handoff shape, and stop conditions.
4. **Isolate writes:** ideally give each writing agent its own Git worktree and branch; otherwise allocate disjoint path ownership in the shared working tree. Never let two subagents edit the same file. Reserve Cargo workspace root manifests, global lockfiles, common schemas, migrations, CI, release metadata and root `AGENTS.md` for the main integrator unless one explicitly delegated change is serialized.
5. **Run independently:** allow parallel research, code and module-local tests. Coordinate scarce resources: serialize heavy Ghidra REA operations on the 16 GB MacBook Air M2; limit simultaneous high-memory Rust builds/GPU tests on Linux; avoid competing writes to the same Cargo `target/` build directory when they cause contention.
6. **Require handoffs:** each subagent returns changed files, precise behavior, tests executed with exit status, evidence for design decisions, any new dependencies/licenses, unresolved issues, and a suggested integration order. Do not accept a prose-only handoff when implementation was assigned.
7. **Integrate deliberately:** main agent reviews diffs and safety constraints, rebases/cherry-picks or incorporates isolated changes, resolves API mismatches, and runs integration tests *after* merging each dependent slice. No autonomous overwrite/reset of user changes.
8. **Verify the vertical slice:** before marking work `VERIFIED`, run targeted unit tests, workspace checks, sample asset roundtrips, GUI test where available, and relevant perf checks; track any skipped verification honestly.
9. **Record and repeat:** update task statuses, active branches/worktrees, commits, integration order and next critical-path tasks. Spawn a new wave only after the shared baseline is coherent. Reuse an agent for follow-up if the client supports it, otherwise create a fresh bounded task.

### Parallelization matrix

- **Good parallel work:** independent crate implementations after API freeze; isolated tests/fixtures; reference documentation; performance profiling vs UI investigation; independent bug diagnosis; exporters for unrelated formats behind common interface.
- **Do not parallelize the same write surface:** public Rust structs/traits, core document schema, central event bus, global Cargo workspace dependencies, project-file format migrations, shared shader entry points, single GUI scene/panel file, or final release tagging.
- **Serial before parallel:** establish stroke/texture representation → then separately implement GPU upload, UI controls and persistence adapters. Establish bake API → then parallelize bake algorithms and validation fixtures.
- **Resource rule:** only **one** heavy REA/Ghidra native investigation at a time on the Mac. Do not confuse REA's ability to inventory binaries with authority to reverse engineer a proprietary program.

### Integration gate — pass or reopen

For each merged work item, verify:

- API/schema compatibility with downstream modules.
- Correct ownership: no unrelated file edits or accidental generator/formatter churn.
- Tests before/after and actual command output recorded.
- No broken `cargo fmt`, tests, Clippy/build introduced by the slice.
- No nonfunctional UI, silent data loss, regressions to Blender/Unity/Godot interoperability, unauthorized assets, or secret leakage.
- Critical cases exercised (UV seams, degenerate triangles, shape transforms, brush overlap, channel isolation, history, save/load as relevant).
- Visual/performance properties tested where hardware and GUI are available; otherwise explicitly noted as **UNVERIFIED**.

A subagent declaring `DONE` means *ready for integration review*, not automatically complete. The parent is the only agent that marks a task `VERIFIED` after the integrated checks pass.

### Persistent team state

Maintain a lightweight task board (`docs/roadmap/agent-workboard.md`) with these fields for every task: `ID`, `stage`, `owner`, `status`, `dependencies`, `owned paths`, `branch/worktree`, `integration commit`, `tests`, `evidence`, `next action`. Status progression: `QUEUED → CLAIMED → IN_PROGRESS → READY_FOR_INTEGRATION → VERIFIED`, with `BLOCKED`/`REOPENED` as needed. Write down why a task is not parallel-safe, rather than spawning idle agents.

Keep `docs/roadmap/implementation-status.md` as the authoritative **product** status. The workboard tracks the agents and their assignments; the implementation-status file tracks what actually works for users.

### Tokens, time, and honesty

Parallelism can reduce elapsed time but consumes more tokens and compute. Never spawn agents just to maximize the number of running threads. Favor `explorer` or medium effort for bounded audits and tests; use High for geometry, concurrency, architecture and subtle Rust ownership/integration defects. The principal agent may choose a smaller team when shared context, RAM, GPU or API availability makes that faster.

A persistent `/goal` is not an unlimited background daemon. Continue as permitted by the active client/session and budget. If interrupted or blocked, preserve a handoff and stop cleanly; do not claim stages S0–S15 are finished without their acceptance evidence.

---

## 1. CONTEXT, FACTS AND VERIFICATION

The owner previously developed Kalium in Rust with an experimental GPU renderer, egui-style panels, documents with layers/channels/masks, brush presets, undo/redo, material bindings, mesh import and workspace serialization. There was a previous passing test suite (~46 tests). **These are historical hints, not proof of the repository's current condition.** Re-check everything on disk before making decisions.

Remote reference environment previously passed a REA connectivity/inventory test:
- MacBook Air M2, macOS ARM64.
- MCP server: `rea` / `rea-agents` version 6.7.0.
- `ghidra` provider: available, v12.1.4; Java 21 and native ARM64 decompiler verified.
- `hopper` may also appear available, but it is paid and **must not be selected**. Explicitly choose `provider_id="ghidra"` when opening native binary targets.
- Painter bundle: `/Applications/Adobe Substance 3D Painter/Adobe Substance 3D Painter.app`.
- Main program: `Contents/MacOS/Adobe Substance 3D Painter`.
- Initial REA inventory evidence: `ev_ef0bfe6dcb8e06a9d68648b86a00ccd665e07b75523b4fefa322cbb78add2ef0`.
- Prior inventory consisted of 47,231 entries; this was a **file inventory, not comprehension of the code**. Reuse evidence and avoid re-inventorying the entire bundle without a concrete reason.

No assumption of GUI automation is allowed: Ghidra static analysis is not a Painter UI driver. A connected MCP server also does not mean a macOS app is automatically controllable. Report observational limitations honestly.

### Reference sources

Use open specifications and published documentation first:
- Substance 3D Painter manual: https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/home
- Painter project/texture set model: https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/getting-started/project-creation
- Painter layer stack: https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/interface/layer-stack/layer-stack
- Painter texture channels: https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/interface/texture-set/texture-set-settings
- Painter baking: https://experienceleague.adobe.com/en/docs/substance-3d/bakers/getting-started/software-interface/substance-3d-painter
- Painter export: https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/export/export-window/export-settings
- glTF 2.0: https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html
- Khronos PBR: https://www.khronos.org/gltf/pbr
- REA runtime contract: https://github.com/morluto/rea/blob/main/docs/mcp-contracts.md

Research must distinguish `observed`, `documented`, `inferred`, and `unverified` facts. Link external notes and evidence IDs without copying disassembled source material into the repository.

### Legal / ethical boundary

The Adobe student license does **not** itself guarantee a right to reverse engineer. Before any native decompilation or invasive observation, check applicable license terms and lawful exceptions/authorization. If legal authorization cannot be established, **do not run that analysis**; use public docs, open-source implementations, public specifications and your own black-box test assets instead. Do not bypass DRM, activation or protections. Never copy Adobe source or decompiled pseudocode, shader code, assets, bundled fonts, designs, bitmaps, presets or proprietary data into Kalium. Implement independently from observable functional requirements and public algorithms, retaining provenance and license checks. Do not ship or publish raw REA results or executable excerpts.

---

## 2. FIRST ACTIONS: REPOSITORY AUDIT (P0)

Execute these steps in order:

1. `cd ~/Dev/Kalium`; record `pwd`, branch, `git status --short`, HEAD hash, Rust toolchain, OS and GPU; never overwrite dirty changes.
2. Inspect root docs, Cargo manifests, `rust-toolchain*`, `AGENTS.md`, tests, examples, CI files, license and previous TODOs. Follow existing repo-local developer instructions.
3. Produce a dependency/module map: document state; paint brushes; compositing; GPU; geometry; viewport; persistence; asset import; plugin system; UI; history; error handling.
4. Identify concrete working user journeys by reading their implementations, **not** by counting declared structs or buttons.
5. Run formatting/lints/tests/build that apply to the actual repo. E.g., `cargo fmt --all -- --check`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` *only if supported*. If existing failures are present, classify baseline vs newly introduced failures.
6. Obtain a reproducible **baseline screenshot or demo scene** if GUI execution is available. Never claim visually correct rendering from a headless test alone.
7. Inventory assets and third-party dependencies, include their licenses and maintenance risks.
8. Create/update the following **tracked Markdown/JSON artifacts inside the repo** (choose appropriate conventional docs paths, without overwriting important existing files):
   - `docs/roadmap/master-goal.md` (this document or an adapted canonical copy).
   - `docs/roadmap/implementation-status.md` (current stages: NOT_STARTED, IN_PROGRESS, BLOCKED, DONE, VERIFIED).
   - `docs/architecture/overview.md` (actual architecture and component contracts).
   - `docs/research/reference-evidence.md` (links to public docs and permitted REA evidence; not proprietary dumps).
   - `docs/qa/test-matrix.md` (acceptance tests, coverage, platform validation).
   - `docs/roadmap/decisions.md` (ADR-like choices and why alternatives were rejected).
   - `docs/roadmap/session-handoff.md` (continuation checkpoint).
9. Build a dependency-ordered task backlog. For every task capture ID, priority, dependencies, files, observable behavior, test/benchmark, status and commit. Start implementation after this bounded audit.

**Exit gate A0:** a baseline that builds/tests to the extent supported, an accurate repository map, no lost user data, and a staged implementation backlog. A status table alone is not sufficient; proceed into implementation.

---

## 3. PRODUCT SPECIFICATION AND FEATURES

### 3.1 Essential workflows — P0 (the application is truly usable)

**P0-A — Project/document lifecycle**
- Create/new/open/save/save-as/close/recent projects; unsaved-change confirmations.
- Native format versioning, data integrity, transactional/atomic saves, autosave, recoverable backups and schema migration tests.
- Store imported mesh references or embedded copies, per-material texture sets, resolution, bit depths, color management, layers/masks, brush settings, sources and export settings, UI workspace as appropriate.
- Document loading cannot execute arbitrary code and must defend against malformed/oversized files and zip/path traversal if archives are used.
- Reopening a project must reproduce a representative scene and its appearance, not only parse metadata.

**P0-B — 3D assets and UVs**
- Import at least one well-documented production format (prefer glTF/GLB) and one traditional interchange format supported by the existing importer (e.g. OBJ). Add FBX when a vetted compatible library/import backend is available; do not promise full FBX fidelity without proof.
- Preserve mesh indices, normals, tangents and handedness, UV sets, material slots, submeshes, topology and transforms; maintain sensible naming.
- Support multiple mesh objects/materials and multiple texture sets; shared or overlapping UVs must not accidentally cross-paint distinct materials.
- Display mesh validity errors. Handle degenerate triangles, missing UVs, out-of-range indices, mirrored UVs and UV islands.
- Preserve skinning/joints, weights, BlendShapes/morph targets when source support and chosen downstream workflow require them; never silently mutate them during import/export. Tests should detect regressions.
- UV viewport showing islands, zoom/pan, checkerboard and texture set selection. Initial 3D painting can require existing UVs, but explain missing UVs to user. No automatic unwrap claim until implemented.

**P0-C — Painting engine and picking**
- Ray-to-mesh picking via acceleration structure/BVH or equivalent, with barycentric interpolation to UV space; correct triangle/material selection and transform handling.
- 3D and UV 2D painting produce consistent texture changes. Handle mirrored UVs, island edges and bleed correctly.
- Continuous strokes with spacing, radius, pressure (mouse fallback), hardness, falloff, opacity, flow, stroke interpolation and alpha stamps.
- Per-channel editing for base color, roughness, metallic, normal, height, emissive, opacity and user-defined channels when supported. User can enable/disable individual channels without losing source data.
- Paint, erase and fill; a practical color/material selector and eyedropper. Clean stroke cancellation and undo/redo grouping.
- Correct compositing mathematics in **linear color**; convert sRGB textures at edges as appropriate. Distinguish scalar channels from color channels, channel masks, alpha semantics, premultiplication and normal-vector blending.
- Texture resolution settings; avoid unnecessary whole-texture uploads for small strokes. Dirty regions or tile-based updates where feasible.
- Deterministic CPU-reference or software-testing path for brush math even if interactive rendering is GPU-based.

**P0-D — Layer stack**
- Paint and fill layers, groups, ordering, rename, visibility, opacity, per-channel blending, solo and enable/disable.
- Non-destructive black/white masks with painting and inversion; layered compositing recalculates only affected areas.
- Group masking and compositing follow explicit algebra; tests for nested opacity, transparent groups, channel-disabled behavior and layer reorder.
- Duplicate/delete/reorder operations are undoable. Actual GUI handles drag-and-drop and selection.
- Keep content when a channel is temporarily disabled, unless an explicit destructive operation is confirmed.

**P0-E — PBR viewport**
- Orbit/pan/dolly framing, select mesh/material, basic studio environments, configurable directional/IBL lighting if feasible, grid and isolation.
- PBR metallic/roughness shading using known standards; normal mapping/tangent basis; correct color management; emissive/opacity behavior if available.
- Updates visible without requiring reopen or full project reload. Avoid shader compilation during every dab.
- 3D and 2D views can be opened together; synchronize selections and paint results.

**P0-F — Import/export for real engines**
- Export PNG/TGA/TIFF/EXR according to existing dependencies and actual bit-depth support. Minimum viable: accurate PNG for colors/scalars, correct normal export and optional float format where applicable.
- Configurable names, sizes, channel packing, padding/dilation, tangent normal orientation (OpenGL vs DirectX Y as appropriate), color space and per-texture-set presets.
- Supply initial templates for **Blender**, **Godot**, **Unity** and generic glTF PBR. Verify packed ORM and channel swizzling; never silently label exports engine-ready if their material conventions are wrong.
- Re-import exported maps or sample them to validate roundtrip fidelity. Do not confuse export image size with working set resolution.

**Exit gate P0:** users can load an original mesh, paint a smooth visible stroke across triangles onto a selected material, compose at least two masked layers, save/reopen without changed output, and export working maps checked in Blender/Godot/Unity (at least one real downstream integration test plus automated format checks).

### 3.2 Non-destructive authoring — P1

**P1-A — Brushes and tools**
- Brush preset serialize/import/export within a documented native schema; pressure/tilt/smoothing when hardware provides it; alpha/stencil textures with safe licensing.
- Symmetry: object/world mirror and optionally radial; 2D projection/stencil, 3D planar, tri-planar and paint-through projection as independently designed.
- Clone, blur, smudge, eraser, straight lines, shape primitives, polygon fill, material ID fill, selection masks and transformations where feasible.
- Per-channel stamp properties, jitter, rotation, angle follow, spacing, anti-aliasing and consistent undo.

**P1-B — Procedural authoring**
- Parameterized effects represented as an explicit graph/stack with serialized type IDs, stable evaluation order, versioned parameters and undo.
- Generators based on baked maps: edge wear, cavity, dirt, position/world-normal gradients, grunge/noise with original procedural algorithms, not copied graphs.
- Filters: levels, histogram/curves, blur/sharpen, hue/saturation, contrast, color balance, threshold and normal/height utilities.
- Anchor references between layers or effects with cycle detection and incremental dependency invalidation.
- Smart material and smart mask *presets* as native editable graph/group templates. Make seed presets original and redistributable.
- Evaluate safely on CPU/GPU with resource limits and caching; preserve deterministic results for tests.

**P1-C — Mesh map baking**
- Bake normal (tangent/world), ambient occlusion, curvature, position, ID, thickness; optional bent normals and height after main maps are correct.
- High-poly to low-poly bake with ray casting, cage or distance parameters, backface/occlusion policies, material/object matching, dilation and supersampling.
- Correct tangent-space conventions and coordinate transforms. Provide visible progress, cancel and re-run; never freeze GUI unboundedly.
- Independent algorithm tests using canonical analytic meshes (plane, cube, sphere, bevelled edge, nested forms) and numeric/image tolerances.
- CPU fallback for correctness; GPU acceleration after a validated baseline.

**P1-D — UV and UDIM**
- UV island visualization/edit-safe reading, seam painting/padding, handling UV tile offsets; distinguish overlapping/mirrored UVs from UDIM tiles.
- UDIM import, multiple tiles, per-tile memory budget, painting across seams, image export naming and persistence.
- Optional auto UV unwrap **only with a vetted, properly licensed library** and documented limitations; do not fake it with flattening or corrupted UVs.

**P1-E — Artist interface**
- Drag/dock/resize/save panels, layer tree, properties, shelf/search/tags, asset browser, texture set list, history, log/progress, keyboard shortcuts, undo/redo UI, command palette if suited to existing UI framework.
- Clear empty states, informative errors, tooltips, task cancellation and autosave/recovery prompts.
- Original Kalium visual identity; do **not** pixel-copy Substance Painter's interface or branding.
- High DPI scaling and keyboard navigation, sensible layout at 1080p and laptop dimensions.

**Exit gate P1:** the user can create a complex fully-editable multi-material texture treatment (fill layers, masks, generator, baked mesh maps), save/reopen, and export it without flattening the editable source.

### 3.3 Advanced and release-ready — P2

- Robust FBX/USD support if realistically maintainable, PBR extensions, multiple UV maps, multi-resolution texture sets and larger materials.
- Asset library with semantic tags, import/export/presets, thumbnails and local index; no cloud account.
- Seam-aware/geometry-aware texture projection, UV layout validation, stencil/clone refinements, 8K export only where supported by memory budgets.
- Comprehensive content-addressed cache, sparse/tiled texture residency, out-of-core streaming, background compaction, incremental recomposition.
- CLI batch mode for headless import/bake/export and project validation, enabling CI reproducibility.
- Crash reporting **local and opt-in**, safe logging and privacy defaults.
- User-friendly installation/distribution for Linux first; then Windows/macOS when supported by real CI/machines. Do not assert platform support from cross-compilation alone.
- Plugin/extension APIs only after stable core types and explicit permission boundaries. Version APIs, resource limits and sandbox untrusted project content; no hidden arbitrary execution.
- Optional verified interoperability readers for third-party project formats **only when legal, documented and supported by real fixtures**. Adobe `.spp` compatibility is explicitly NOT guaranteed. Prioritize standard texture interchange; do not reverse engineer restricted binary formats or claim full compatibility from recognizing HDF5 signatures.

**Exit gate P2:** documented beta/1.0 criteria, repeatable releases, functional installation, resilient projects and performance data on target hardware.

---

## 4. ARCHITECTURE EXPECTATIONS (ADAPT TO THE ACTUAL REPO)

Do not create crates for their own sake. Prefer explicit bounded subsystem contracts; split crates/modules only when helpful:

- **App/UI:** commands, observable state, docking and workflows; must not own GPU resources or serialize UI pointers.
- **Document/domain:** scene, mesh references, TextureSet, Channel, LayerNode, Mask, BrushPreset, Parameter, revision IDs, immutable snapshots where useful.
- **Geometry:** import, triangulation assumptions, normals/tangents, BVH/raycast, barycentric & UV projection, tile addressing, mesh maps.
- **Paint engine:** stroke state machine, sampling/stamps, compositing, interpolation, dirty region tracking, deterministic reference CPU implementation.
- **Renderer:** GPU abstraction, PBR viewport, offscreen 2D texture compositor, textures, device loss recovery, shaders, resource lifetime.
- **Procedural graph:** typed nodes, cache invalidation, topology dependencies, cycle prevention, parameter persistence.
- **Bakers:** geometry-derived maps, scheduling, progress/cancel, test oracle.
- **Project I/O:** versioned schema, binary blobs, migration, atomic save/restore, validation.
- **Interchange:** format- or engine-specific adapters, explicit precision/color/normal convention.
- **History:** commands/transactions, coalesced brush strokes, memory budget and reconstructible checkpoints.
- **Infrastructure:** error/telemetry (local), concurrency, task queue, cache and incremental rebuild.

### Data and rendering invariants

1. Mesh data and paint/edit data are separate. Painting must not silently modify UV layout, normals, topology, bone weights or morph targets.
2. Layer content is retained independently of visibility/blend/channel enable flags.
3. TextureSet identity and material assignment are stable; one material's edit cannot silently affect another.
4. All color conversion boundaries are documented; sRGB and linear values must not be composited interchangeably.
5. Different channels have different mathematics: scalar blending, normal-vector blending, alpha/opacity, height; avoid applying RGB rules everywhere.
6. Background I/O, baking and compilation must not block UI event processing unnecessarily.
7. All GPU resources are rebuildable when a device is lost; document remains authoritative.
8. Avoid whole-project cloning on brush move; use region/tile deltas and bounded undo snapshots.
9. Path handling works with spaces/Unicode. No process command injection from project or asset names.
10. No AI inference or remote network dependency in runtime, test harness or export path; optional update checks, if added, must be opt-in and separate.

### Design review checkpoints

Before introducing a significant architectural change, write a brief rationale including problem, alternatives, migration, performance implications, regression risks and tests. Favor evolving existing tested models over total rewrites.

---

## 5. IMPLEMENTATION PROGRAM — DEPENDENCY-ORDERED STAGES

Implement the following stages in dependency order; small substeps may overlap where no dependency is violated. For **every** stage, deliver a working feature exposed in the product, focused tests, docs and reproducible evidence.

### Stage S0 — Baseline, scaffolding and acceptance fixtures
**Tasks:** perform section 2; create original synthetic test meshes (plane, UV cube, sphere, bevels, multiple materials), original texture maps, deterministic reference images, stable example project and a minimal GUI smoke test if technically possible. Establish `cargo test`, lint, formatting and offline smoke scripts in CI.
**Acceptance:** fresh checkout can build/test; representative original assets are redistributable; original behavior baseline is captured.

### Stage S1 — Stable mesh + material + TextureSet model
**Tasks:** support exact primitive/material identity, UV channels and transforms; introduce validation and explicit errors, preserve index/morph/skin data where possible; link materials to texture sets; make color space and texture formats explicit.
**Acceptance:** two materials sharing UV coordinates remain independently paintable; malformed meshes fail gracefully; geometry roundtrip invariants hold.

### Stage S2 — Picking and UV hit mapping
**Tasks:** BVH/raycast, visibility/front/back selection, barycentric UV/normal/tangent interpolation, transformations, 3D/2D view coordination, UV islands, overlapping-UV policy.
**Acceptance:** tests with analytic triangles, transformed meshes, seams, degenerate geometry and mirrored UVs; picking returns material ID + texture set + UV with bounded error.

### Stage S3 — Continuous real-time painting
**Tasks:** brush samples/resampling/interpolation, stamps, falloff, spacing, blend math per channel, in-memory texture writes, dirty rectangles, GPU display refresh, brush cursor, undo grouping, cancellation.
**Acceptance:** mouse down/drag/up creates one visible continuous stroke in viewport and UV view, undo reverses it, redo reconstructs it, no significant holes across adjacent triangles on normal test mesh.

### Stage S4 — Multichannel PBR and non-destructive layers
**Tasks:** channels/color conversion; layer and group compositor; fill, mask, opacity, blend modes; masking order; incremental invalidation; deterministic regression images; real GUI layers pane.
**Acceptance:** original layer samples survive hide/disable; complex masking matches tested compositing results; channels remain independent; changes appear live.

### Stage S5 — Project persistence + autosave/recovery
**Tasks:** schema versioning, atomic write, reader bounds, document migration, background save, autosave/journal or checkpoints, crash recovery workflow and corrupted-file error messaging.
**Acceptance:** open-save-reopen is pixel- and metadata-stable for layered projects; simulated interrupted write leaves a recoverable valid revision; older fixture migrations are tested.

### Stage S6 — Practical exports and engine presets
**Tasks:** image encoding/bit depths, padding/dilation, rescale options, filename rules, channel packing, normal green channel conversion, documented presets for Blender/Godot/Unity/glTF.
**Acceptance:** export outputs expected resolution and channels; use image pixel assertions; validate at least one real import in a downstream engine where available.

### Stage S7 — PBR viewport polish and production ergonomics
**Tasks:** metallic/roughness shading, normals, environment, camera controls, UV and channel inspectors, material isolation, color management controls, property/asset browser.
**Acceptance:** basic reference PBR scenarios render plausibly, controlled preview image tests detect breakage, no stale texture rendering after edit.

### Stage S8 — Mesh map baking foundation
**Tasks:** robust geometry sampling and acceleration; normal, world normal, AO, ID, curvature, position, thickness, dilation, cancellation and high-to-low projections/cages where needed.
**Acceptance:** analytic bake fixtures with numeric/image tolerance, bake maps usable by procedural generators, cancel leaves document valid.

### Stage S9 — Procedural graph, effects and smart materials
**Tasks:** graph schema, per-layer/mask stacks, fill transforms/projection, filters, generators, anchor refs, deterministic noise, cache dependency invalidation, native presets.
**Acceptance:** edge-wear/dirt generator varies with baked curvature/AO and persists; parameter edits update only affected outputs; cycle is rejected.

### Stage S10 — Advanced brush workflows
**Tasks:** alpha/stencil, pressure curves, symmetry, projection variations, clone/erase/smudge/blur, user presets, brush optimization and edge-aware operations.
**Acceptance:** original symmetry/pressure fixtures; zero regressions to basic strokes; recoverable, edit-safe tools.

### Stage S11 — UV/UDIM and complex projects
**Tasks:** seam-safe painting, UV tile selection, UDIM addressing, per-tile residency/cache, handling large multiset assets, mesh validation.
**Acceptance:** painting and exports across two or more UDIM tiles correct, autosave robust under memory constraints and material assignments stable.

### Stage S12 — Full artist GUI and asset library
**Tasks:** original UI identity, shortcuts, docking, workspace persistence, searchable presets, history, dialogs, progress/cancellation, tutorials and accessibility/hidpi basics.
**Acceptance:** target artist journey can be completed without editing JSON or running developer commands.

### Stage S13 — Performance and scalability
**Tasks:** profile hot paths; minimize per-dab data copies; tile compositing; GPU dispatch and memory reuse; asynchronous jobs; cache budgets; minimize startup cold path.
**Acceptance:** reproducible benchmark report before/after; no correctness regression; UI responsive during typical 2K-4K edits on available target hardware; no claim of 8K support without test.

### Stage S14 — Interoperability and native format polish
**Tasks:** vetted FBX/USD/glTF roundtrip where supported, texture export extensions, engine presets, optional batch CLI, native format migration hardening. Third-party `.spp` support only if lawful, test-based and feasible; otherwise mark unsupported explicitly.
**Acceptance:** documented format matrix of tested read/write features, preservation tests, no silent loss of rigging/UVs/morphs when advertised.

### Stage S15 — Release engineering and QA
**Tasks:** full regression, dependency license scan, reproducible builds, packaging, cross-platform smoke tests for actually available machines/runners, release notes, known limitations, user guide, installation and recovery instructions.
**Acceptance:** install/run on Linux, create→paint→mask→bake→save→reopen→export, clean uninstall and project recovery, signed/checksummed artifacts as supported, no undisclosed critical failures.

**Critical sequencing rule:** Do S0–S6 fully before enthusiastically expanding S8–S14. A smaller working painter beats a giant list of non-functional stubs.

---

## 6. TEST PROGRAM (MANDATORY)

### Unit tests
- Brush interpolation, pressure/flow/opacity, alpha, subpixel stamping, no missing dabs.
- Triangle and ray intersection, barycentric interpolation, UV transforms and seam behavior.
- Color conversions linear ↔ sRGB, alpha premultiplication, normal renormalization, height and scalar channel blend operators.
- Layer/mask tree evaluation, group pass-through or isolated semantics, invalidation and channel retention.
- Archive/file validation, schema migrations, path handling, invalid UTF-8 where possible, huge dimensions/overflow checks.
- Deterministic procedural generators and bake samples.

### Property/fuzz tests
- Save-load-save invariants; no panics on malformed project/mesh/config inputs.
- Reordering/undo/redo equivalence across valid commands.
- Paint strokes outside mesh or missing UV cause no unintended edits.
- Repeated open/save should not monotonically leak resources.

### Integration and visual tests
- Fixture 1: sphere with basecolor + roughness brush strokes.
- Fixture 2: two material sets with overlapping UV, no cross-material paint.
- Fixture 3: nested layer group + black mask + fill, roundtrip and export.
- Fixture 4: AO/curvature bake feeding a generator, distinct predictable outputs.
- Fixture 5: mesh with mirrored UVs, UDIM tiles and seams (when those phases are enabled).
- Fixture 6: malformed/partial save recovered without deleting the last good file.
- Fixture 7: renderer device loss/recreate, if test infrastructure permits.
- Fixture 8: exported maps imported in Blender/Godot/Unity according to advertised profiles.

Use original or clearly redistributable fixtures; never package Adobe's bundled samples or textures.

### Performance benchmark policy
Record device, driver, operating system, input project, resolution, measured steps, baseline and results. Target smooth interactive experience on the owner's desktop class (RTX 3060 12GB, Ryzen 5 5500) while remaining viable on weaker hardware, but **do not assert an unmeasured FPS target**. Track brush latency p50/p95, frame times, VRAM/RAM, import time, save/load time, first-stroke latency, bake duration and cache hit rate. A measurement is not a success unless graphics are correct.

### Security and reliability
- Fuzz untrusted import/parsing paths and guard against oversized dimensions, recursion depth and malicious paths.
- Project files must not execute scripts/plugins automatically.
- Plugins require a defined trust/permission model; do not download or run remote code automatically.
- Explicit recovery on interrupted save, GPU crash or device reset where possible.
- No telemetry/AI/network requirement by default.

---

## 7. HOW TO USE `rea-mac` — RESEARCH PROTOCOL

REA is a *supporting research tool*, not the sole source of architecture. Apply the following efficient workflow only when a task has a genuine research question:

1. State the exact **question** (e.g. documented behavior of multiple TextureSets, UV seam handling) and the reason why open standards/current Kalium tests do not already resolve it.
2. Consult Adobe's public docs, Khronos/graphics references and existing open-source implementations first; derive a user-visible behavioral requirement.
3. If authorized, check MCP connection health and target identity. Reuse evidence `ev_ef0bfe6dcb8e06a9d68648b86a00ccd665e07b75523b4fefa322cbb78add2ef0` for the existing bundle inventory when relevant.
4. If legally permitted and strictly necessary, use REA `open_binary` with the **explicit** Ghidra provider selector `provider_id="ghidra"`, for one relevant executable/library. Never silently choose Hopper or a paid engine.
5. Limit native requests to targeted questions; don't bulk decompile the 142 MB main executable or all 47,231 inventory entries. Track Ghidra cold-start and query timeouts separately. Close sessions and release resources.
6. Record claims as `DOCUMENTED`, `OBSERVED`, `INFERRED` or `UNKNOWN`, with source URL/evidence ID and date. Raw pseudocode/disassembly is not allowed in Kalium sources, docs, comments or commits.
7. Create an **independent functional spec and test fixture**, then implement in original Rust/shaders from public-domain/compatible algorithms.
8. Test Kalium against that spec. Unverified behavior remains explicitly unverified.
9. If REA fails, is unavailable or may violate the license, continue using public specs and original tests; do not block the entire app on static analysis.
10. Do not ask REA to control GUI components that its actual provider doesn't support. To measure app behavior, use authorized manual experiments / separately available GUI tooling, not guesses.

**Remote resource policy:** MacBook Air M2 has finite memory, power and thermal capacity. Avoid simultaneous massive decompilation sessions; close ephemeral Ghidra sessions, bound expensive operations and never store giant binary analysis dumps inside Kalium.

---

## 8. CODE QUALITY AND CONTRIBUTION CONTRACT

- Follow repository-specific edition, MSRV, module naming, formatting and architectural conventions.
- Keep public types documented and avoid hidden global mutable state.
- Prefer Rust safety; use `unsafe` only with isolated invariants and tests. Use bounded inputs and intentional error types rather than unchecked `unwrap`/`panic` on user content.
- Work in small, independently buildable changes. Do not shotgun-change dozens of files to silence compiler errors.
- Every user-visible feature requires a working GUI command path and no dead menus.
- Never permanently mute tests, ignore lint globally, remove asserts or replace real work with hard-coded demo behavior.
- Mark partial features and experimental backends honestly; disable incomplete UI actions when necessary.
- Use compatible OSS dependencies and record their licenses; do not embed proprietary Adobe library code, shaders, samples or assets.
- Preserve repository AGPL licensing and required notices. Treat external code as untrusted and check reuse licenses.
- Keep generated binaries, captures and large fixtures out of Git unless explicitly designed/tested and legally redistributable.
- If upgrading dependencies, benchmark/verify affected GPU behavior and preserve deterministic lockfiles.

### Recovery from failures

When an implementation fails: identify the smallest failing test, classify environment/tool vs code defect, roll back **only your own uncommitted broken slice if necessary**, retain a structured note, implement an alternative. Never `git reset --hard` on user changes; never delete a project file to make a test pass.

---

## 9. FEATURE ACCEPTANCE MATRIX

Maintain a status table in the repo. Use **VERIFIED** only when all applicable criteria are met.

| Feature | Core algorithm | End-user UI | Save/reopen | Automated test | Real visual test | Export/integration |
|---|---|---|---|---|---|---|
| Import + TextureSets | required | required | required | required | required | where relevant |
| Picking + UV paint | required | required | required | required | required | texture export |
| PBR channels | required | required | required | required | required | required |
| Layers + masks | required | required | required | required | required | required |
| Autosave/recovery | required | required | required | required | smoke | n/a |
| Bakers | required | required | required | required | required | through exports |
| Procedural effects | required | required | required | required | required | required |
| UV/UDIM | required | required | required | required | required | required |
| Asset presets | required | required | required | required | smoke | native asset export |
| Cross-platform | required | required | required | required | required | packaged application |

A status such as `DONE` must not override missing verification; distinguish coded, integrated, tested and verified states.

---

## 10. PROGRESS LOG, HANDOFF AND SESSION CONTINUATION

At the **beginning of every session**:
1. Read `docs/roadmap/session-handoff.md`, `implementation-status.md` and `decisions.md` if present.
2. Check `git status`; inspect pending changes; run a short baseline smoke test.
3. Resume the highest-priority unblocked work item whose dependencies are verified.

At the **end of every coherent implementation slice**:
1. Run relevant tests/lint/build, and GUI validation when possible.
2. Update status and evidence, list exact changed files and the actual behavioral improvement.
3. Commit the slice only when safe, following local Git policy.
4. Update the session handoff with the next concrete implementation actions.
5. **Continue to the next slice**, rather than stopping with a summary if the session can still perform work.

`session-handoff.md` should always contain:
- Git branch and HEAD; dirty files and why they are dirty.
- Stage/task ID and DONE/VERIFIED/BLOCKED states.
- Exact relevant commands run and test counts/results.
- Known breakages with reproduction and fix hypothesis.
- REA evidence IDs consulted (no proprietary content), or why research was skipped.
- Top 3 next edits, including files/modules and acceptance tests.
- Any real blockers or decisions reserved for the user.

### Suggested status update format

```
KALIUM DELIVERY CHECKPOINT
Stage: Sx / Task: <stable ID>
Implemented: <actual behavior>
Verification: <commands; pass/fail; graphics check>
Performance: <measurements or not measured>
Git: <branch/hash; uncommitted changes>
Research: <documented/observed/unknown; references>
Risks: <remaining concerns>
Next action: <specific code changes, no generic planning>
```

---

## 11. STOP CONDITIONS AND BLOCKERS

Continue implementing as long as meaningful unblocked work remains. Stop or request authorization when:
- A change would discard or overwrite user work.
- Legal permission is required for protected binary analysis or proprietary format reuse.
- A proprietary license, payment, password, cloud authentication or macOS action outside the accessible MCP is needed.
- The environment prevents compiling/testing any useful slice and no safe local workaround exists.
- A hardware-dependent visual validation cannot be performed; mark it **UNVERIFIED** and work on testable portions.
- A migration has a realistic risk of irreversible user data loss without a verified backup.

When blocked, make partial progress in independent modules, leave a concrete handoff, and clearly explain the exact blocker. Do not misreport completion and do not ask the user to provide facts already established in this repository or in the remote MCP.

---

## 12. PRODUCT-LEVEL DEFINITION OF DONE

**Functional:** An artist can complete the end-to-end PBR texturing workflow with real imported 3D meshes; edit non-destructively; bake usable maps; save/recover; and export maps to at least one proven external application. At P1/P2 maturity, tools/effects/multi-material/UDIM and robustness are demonstrated—not just planned.

**Quality:** Relevant tests pass; deterministic test assets and known tolerances are documented; important UI flows are exercised; no known critical data corruption; warnings are investigated rather than blanket-suppressed.

**Performance:** Benchmarks exist on a named GPU/system with reproducible settings. Interactive responsiveness meets a measured, explicitly defined budget appropriate for real projects; performance tradeoffs documented.

**Engineering:** Clean architecture docs, maintainable Rust code, no unauthorized proprietary source/assets, valid AGPL/third-party attribution, stable persistence format or migrations, clear known limitations, repeatable packaging.

**Honest status:** Feature coverage is not declared 100% unless all acceptance criteria, end-user journeys and platform tests actually pass. A project need not replicate every proprietary feature or exact user interface to be a successful independent Substance-Painter-class application.

---

## 13. START NOW — DIRECT EXECUTION ORDER

**Do not respond with only a plan.**

1. Locate `~/Dev/Kalium`; run the repository baseline checks and report discovered architecture in `docs/architecture/overview.md`.
2. Determine the smallest missing dependency for an end-to-end import → pick → paint → UV preview → PBR channel → save → reopen → export path.
3. Add a focused regression fixture/test for that dependency.
4. Implement it in production Rust code, integrate with current UI, and verify it.
5. Repeat for the next dependency through S0–S6; then proceed to advanced stages in their documented order.
6. Keep the progress/handoff files current. Whenever you need reference clarification, follow the constrained research protocol; **do not make REA a mandatory blocker for normal engineering**.
7. In your next user-facing update, show **what changed and what passed**, not only ideas for future work.

**Your mission is to build Kalium, not to merely describe how Kalium could be built.**

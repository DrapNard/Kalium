# Architecture and execution decisions

## D-001 — Keep the Rust/egui/wgpu application architecture

**Decision:** retain the single Rust binary and current eframe/egui_dock/wgpu UI shell while building feature slices.
**Reason:** it is the repository's functioning architecture; the current need is correctness and workflow completion, not a framework rewrite. The CPU 3D rasterizer remains an explicit reference implementation.
**Status:** active.

## D-002 — Keep workspace layout outside project files

**Decision:** serialize dock state to the per-user configuration directory, not `.kalium`.
**Reason:** layout is an editor preference and must not alter or hide project content. The View menu can restore closed panes/default layout.
**Evidence:** `src/layout.rs`, including workspace serialization tests.

## D-003 — Native `.kalium` is the editable source of truth

**Decision:** preserve an open, versioned ZIP/JSON/PNG project format and treat third-party compatibility as a separate clean-room effort.
**Reason:** native editable layers and masks can be tested without copying proprietary implementation or resources. `.spp` signature detection is not import.
**Status:** active; validated temp recovery and pre-decode image bounds are implemented. A bounded, identity-bound recovery journal now replays supported image and layer edits; background writes, complete mutation coverage, durability guarantees and broader fuzzing remain.

## D-004 — Preserve and review the pre-existing dirty worktree

**Decision:** do not reset, rewrite or overwrite pre-existing work. Review and isolate changes before staging; commit reviewed deliverables when the active task explicitly asks for maintained commits.
**Reason:** the initial worktree already contained modified Rust files, an untracked Cargo.lock and an untracked master goal. Preserving those changes and honoring the user's later explicit request for commits both matter.
**Status:** the first reviewed CI/lockfile slice was committed as `12bc283`; the broader implementation is staged separately after integration checks.

## D-005 — Handle unsaved document transitions in the UI

**Decision:** New, Open and native window close must defer destructive transitions until Save, Discard or Cancel is chosen; a failed/cancelled save must keep the document open.
**Reason:** the current UI silently replaces dirty content.
**Status:** implemented with policy tests for New/Open/native-close, save cancellation/failure and discard behavior. Desktop interaction remains visually unverified.

## D-006 — Make temp-file recovery explicit and non-destructive

**Decision:** when the primary project cannot be fully validated, allow Open to fall back to a fully validated sibling `.tmp` archive. Do not promote it automatically or overwrite the primary; mark the recovered document unsaved and require Save As.
**Reason:** a partially written or stale temp is untrusted, and recovery must not silently destroy the last primary revision. A valid primary always takes precedence.
**Limits:** this is a recovery path for a complete validated temp archive, not an autosave journal or a power-loss durability guarantee.
**Evidence:** project recovery tests plus app-state tests for recovered/primary open paths.

## D-007 — Keep UV edge padding opt-in and material-set scoped

**Decision:** offer per-set nearest-covered-texel padding as an export option, with zero padding preserving the legacy output exactly. Cap radius at 256 texels and seed the transform only from the set's own rasterized UV coverage.
**Reason:** edge dilation reduces texture-filtering seams while preventing one material set from bleeding into another; opt-in zero retains existing export expectations.
**Limits:** padding is not a complete engine preset, does not add bit-depth/rescaling controls, and has not been validated in Blender/Godot/Unity.
**Evidence:** brute-force Euclidean nearest-source oracle, pixel checks for the gutter, set-isolation and byte-for-byte radius-zero tests.

## D-008 — Composite color-valued PBR channels in linear light

**Decision:** treat Base Color and Emissive RGB as sRGB-encoded color values for layer compositing: decode to linear light, apply Normal/Multiply/Add, interpolate by source alpha × layer opacity × mask, then encode the result back to sRGB. Treat Normal as a vector orientation and scalar channels as normalized data values; do not apply color transfer functions to those channels.
**Reason:** color-channel blends should operate on light values, while scalar texture channels and normals carry data with different semantics.
**Evidence:** numeric Base Color and Emissive brush/layer blend tests, normalized Normal interpolation and detail-normal tests, and exact packed export source-channel tests.

## D-011 — Use vector-aware Normal painting and detail composition

**Decision:** Normal brush strokes decode to signed vectors, interpolate by straight-alpha coverage, normalize, and encode. Existing Normal layer blending remains normalized orientation interpolation; the explicit Detail Normal blend uses reoriented normal mapping after weighting the detail vector against neutral by pixel alpha × layer opacity × mask. Other channels interpret Detail Normal as ordinary Normal blending.
**Reason:** normal maps encode directions rather than color intensities. Separating an explicit detail mode preserves existing layer behavior while allowing artist-controlled fine surface detail.
**Evidence:** numeric brush tests for repeated stamps and transparent/erase behavior; compositor tests for RNM, normalization, and alpha/opacity/mask identity.

## D-012 — Make Normal export Y convention explicit

**Decision:** store and composite Normal maps in the OpenGL (+Y) convention. Export can retain OpenGL bytes or invert only the green channel for DirectX (−Y), after optional UV padding. The all-texture-set GUI export exposes the choice; no conversion is implicit in ordinary per-map compositing.
**Reason:** engines differ in tangent-space Y orientation, and silently guessing creates visible shading inversions. An explicit export control preserves existing output defaults.
**Evidence:** exact-pixel assertions compare default OpenGL bytes and DirectX output for both active-set and all-set APIs, including unchanged red, blue, and alpha bytes.

## D-013 — Keep manual recovery checkpoints separate from project transactions

**Decision:** store one explicit user-created recovery checkpoint at the per-user config path `recovery-v1.kalium`, independent of the selected project and its sibling `.tmp` replacement file. The checkpoint is global across projects; the File menu labels recovery as global. Mark restored content unsaved so Save As preserves it, and reject checkpoint saves when this path is also the open primary project.
**Reason:** a long-lived checkpoint must not be mistaken for an interrupted primary save. Recovery is an explicit user action, and path separation prevents writing the checkpoint into the active primary. Timed autosave and background snapshotting require a separate memory-bounded design.
**Evidence:** checkpoint round-trip verifies pixels, masks, sets, bindings and unknown metadata while asserting primary project bytes remain unchanged; app recovery reuses the Save As policy.

## D-014 — Use deterministic CPU GGX as the viewport reference

**Decision:** shade Base Color and Emissive in linear light with a Cook–Torrance GGX direct light, Smith masking, Schlick Fresnel, and a constant Lambertian ambient fill. Apply AO only to the ambient fill; preserve raw channel bytes in solo views. Keep this path as the CPU reference rather than claiming native GPU rendering or IBL.
**Reason:** artist preview should respond to metallic, roughness and tangent normals using a known microfacet model while keeping numeric behavior testable without graphics hardware.
**Evidence:** analytic dielectric/metallic/roughness/grazing oracles, AO isolation, finite input bounds, sRGB transfer, deterministic frame equality and a fixed center-pixel tolerance.

## D-015 — Capture stroke undo before-images by touched UV tiles

**Decision:** remove document-sized snapshots from interactive brush strokes. Before each stamp mutates a selected layer channel or mask, capture each intersected 64×64 UV tile only once. At stroke completion, retain row-strided before/after patches for changed tiles. Bound projected patch storage by the existing 96 MiB history cap; if a single stroke exceeds it or allocation reservation fails, stop that stroke and restore every captured tile so existing undo and redo remain valid.
**Reason:** a small dab should not clone and scan an entire 4096² image. Tile before-images keep overlap handling simple, deterministic and independent of individual brush blend modes.
**Limits:** candidate stamp bounds can capture tiles whose pixels are later skipped by coverage or zero opacity. A stroke that exceeds the budget is canceled as a whole. Fill operations keep their existing full-snapshot path until they receive a separate region-aware implementation.
**Evidence required:** 4K small-dab capture budget, repeated tile capture once, exact undo/redo pixel reconstruction, mask capture, and oversized stroke preserving earlier undo history.

## D-016 — Reorder layer rows with stable IDs and one undo entry

**Decision:** expose layer rows as egui drag sources and drop targets. A drop moves the source layer to the target's stack index while preserving the selected layer by ID, then records the operation as one bounded undo/redo entry. Self-drops and invalid indices do nothing.
**Reason:** the artist-facing Layers pane must support direct stack editing; toolbar arrows alone do not satisfy the required drag-and-drop workflow. Stable IDs keep selection independent of row position.
**Evidence:** a headless egui pointer-event test drags between actual rendered layer rows and verifies order, stable selection, dirty state, undo and redo. Desktop visual interaction remains unverified.

## D-009 — Derive the separate Base Color alpha export from Opacity

**Decision:** write `Kalium_BaseColor_Alpha.png` with Base Color RGB and set its alpha byte from the composited Opacity channel's red byte. Keep the ordinary `Kalium_BaseColor.png` output as the unmodified Base Color composite.
**Reason:** the Opacity material channel is the explicit source of exported surface opacity; internal straight-alpha coverage in Base Color layers describes paint storage and is not a substitute for that material channel.
**Evidence:** `src/export.rs::write_maps` and `packed_engine_maps_use_exact_source_channels` assert the Opacity-derived alpha while Base Color layer alpha differs. Downstream application interpretation remains unverified.

## D-010 — Transform imported normals with the inverse transpose

**Decision:** when baking glTF node transforms into mesh vertices, transform normals by the inverse transpose of the world transform and normalize them; positions use the ordinary point transform.
**Reason:** nonuniform scale must leave transformed normals perpendicular to their transformed surface tangents.
**Evidence:** `gltf_nonuniform_scale_uses_inverse_transpose_for_normals` checks the analytic direction, unit length, finiteness, and tangent perpendicularity.


## D-017 — Recompose only touched paint tiles in the active map cache

**Decision:** during interactive painting, collect unique 64×64 tiles touched by brush stamps, composite only those rectangles for the edited Texture Set and channel, then patch the cached material map and active egui UV texture. Preserve full-cache invalidation for structural, fill/clear, and non-paint edits.
**Reason:** stroke undo already identifies bounded changed regions; recompositing every channel in every Texture Set after each dab would discard that locality.
**Limits:** this does not make CPU viewport rasterization incremental and has no end-to-end latency benchmark. Other edit paths may still rebuild full composites.
**Evidence:** cropped composites match full composites byte-for-byte across channels and layer semantics; app cache patching matches a full composite. Desktop texture-upload behavior remains unverified.


## D-018 — Persist stable project identity for recovery association

**Decision:** native manifest v6 stores a non-nil UUID on `Document`; v1–v5 load by assigning a new in-memory UUID, which becomes stable when the migrated project is saved. Save As to a distinct project path forks the identity; an ordinary save keeps it.
**Reason:** recovery records must be associated with the project and saved baseline that produced them, rather than a single global checkpoint or path alone.
**Limits:** Save As branches get separate journal identities. Recovery covers committed image edits and selected layer properties/order/selection; oversized patches and remaining document mutations disable the journal until the next save so partial recovery is never offered. An unsaved legacy project has no persisted identity in its original archive until saved as v6.
**Evidence:** v4 migration writes a stable UUID through v6 save/reopen; the bundled v1 fixture loads with an assigned identity; nil v6 UUID is rejected. Journal v2 validates UUID, schema, and the bounded SHA-256 archive fingerprint.


## D-019 — Journal forward pixel and mask patches as bounded events

**Decision:** encode a layer image rectangle patch as one bounded JSON event with Texture Set ID, stable layer ID, target channel/mask, rectangle, and exact stored bytes. Replaying validates IDs, dimensions, row layout and projected decoded texture budget before mutating a candidate document.
**Reason:** brush and mask edits naturally map to small deterministic regions, and replay must not reinterpret brush settings or depend on the viewport.
**Limits:** image events cover committed strokes, snapshot-based edits, and imported texture layers. Layer property, reorder, selection, blank-create, selected-layer duplicate/remove, Texture Set create/remove/rename/select/resize and material binding events are supported; mesh and undo/redo mutations remain unsupported and disable recovery until a new save. A single JSON event batch may not exceed the 8 MiB frame payload; ordinary history extraction limits raw patch bytes to 1 MiB, while imported channel strips are capped at 256 KiB per record. App appends synchronously on the UI thread; desktop modal and process-crash validation remain open.
**Evidence:** exact active/inactive-set channel and mask tests round-trip through the bounded codec; malformed bounds and over-cap JSON fail without applying a patch. Headless app tests cover save→append→reopen→Restore for image and supported layer edits, Discard, unsupported mutation fail-closed behavior, and truncated-tail prefix recovery with append continuation, and complete checksum-corrupt frame rejection without a recovery prompt.

## D-020 — Replay bounded layer create, duplicate and remove operations

**Decision:** journal creation of an empty layer with only its Texture Set ID, next stable layer ID, final stack index and name. Reconstruct its deterministic default properties, empty channels and opaque mask during replay. Validate all bounds and decoded mask bytes before mutation; preserve selection by ID, then record the app's new selected layer separately. A duplicate event stores the Texture Set ID, selected source layer ID, new stable layer ID and final insertion index. Replay derives the existing `source name + " copy"` name and preserves the complete layer state. A remove event stores the Texture Set ID, removed stable layer ID and replacement selected ID; replay validates both IDs and removes the currently selected target.
**Reason:** Add has a deterministic low-payload representation. Removing a layer needs no removed-layer payload because forward replay applies the destructive operation to the matching saved baseline, where the removed content is already present. Duplicate reuses the selected source at that exact point in the event stream, validates decoded image/mask storage and a 48 MiB aggregate serialized layer-extension copy limit, and clones before document mutation. Mesh import and undo/redo remain fail-closed; Texture Set and material binding events have separate stable-ID contracts.
**Evidence:** active/inactive-set create/duplicate/remove replay, stable selection, invalid/no-mutation and resource-bound checks; app Save→Add/Duplicate/Remove→reopen→Restore tests leave journal recovery enabled.

## D-021 — Select one deterministic glTF scene when the asset omits a default

**Decision:** import the declared default glTF scene when present; otherwise import the first declared scene only. Do not merge alternative scenes into one mesh.
**Reason:** scenes are alternate node-graph views, and merging them may duplicate shared nodes or combine geometry the asset author intended to keep separate.
**Evidence:** generated glTF fixture has a shared node across two scenes; explicit default selection imports its two nodes once, while the no-default fallback imports the first scene once.

## D-022 — Preserve triangle orientation under reflected glTF transforms

**Decision:** after composing parent and node transforms, reject singular or non-finite transforms. For a negative determinant, swap the second and third index of each imported triangle while preserving its material-slot entry.
**Reason:** positions are baked into mesh space; leaving winding unchanged under a reflection reverses geometric normals and disagrees with inverse-transpose supplied normals. Stable orientation matters for generated normals, shading, and backface-sensitive future paths.
**Evidence:** generated- and supplied-normal reflected fixtures assert corrected +Z orientation, retained material assignment, expected UV/material picking, and singular-transform rejection. Nested/mirrored transform fuzzing remains open.

## D-023 — Stream imported texture recovery as bounded row strips

**Decision:** bracket imported-channel recovery with `TextureImportBegin` and matching `TextureImportCommit` records. Inside the transaction, journal a `LayerCreate`, full-width channel patches capped at 256 KiB raw per row strip, then final layer selection. At startup, validate the transaction's event order and matching IDs; if the journal ends before Commit, truncate the incomplete final transaction while preserving earlier records. On allocation, encoding or append failure, persist the disabled marker.
**Reason:** an imported channel may be much larger than the 8 MiB JSON frame cap. Full-width strips bound each record, while the transaction boundary prevents process termination between synced strips from exposing a partial image.
**Limits:** numeric JSON expands patch bytes and the total journal is capped at 256 MiB; a large import may exceed that cap and then fail closed. Appends synchronously flush on the UI thread. The transaction envelope currently covers imported textures only.
**Evidence:** a 1024² import produces 16 patch records and restores exact resampled pixels, channel, name and selection. Complete/incomplete/malformed transaction tests check truncation and append continuation. A child process killed after LayerCreate and one strip restores a prior edit without replaying the incomplete import.

## D-024 — Verify CLI artifact generation in CI

**Decision:** run a shell smoke script against the bundled demo after building the CLI. Verify project validation/inspection, preview, UV coverage, AO, mesh-map and all 11 export outputs, PNG signatures/dimensions, mesh-map metadata, and unchanged source-project SHA-256.
**Reason:** successful CLI exit codes alone do not prove the expected artifacts exist or that read-only commands leave the input untouched.
**Limits:** artifact checks do not validate image appearance, desktop GUI behavior, or Blender/Godot/Unity import. Hosted CI remains unverified in this environment.
**Evidence:** `scripts/qa/cli_release_smoke.sh` passes locally and is wired into the Linux CI job.


## D-025 — Verify per-material Texture Set export paths in Blender

**Decision:** retain the existing single-set Blender smoke and add a reproducible two-set fixture. Map exact OBJ material slot names to distinct relative export folders, assign one Principled material per slot, save/reopen the `.blend`, and verify each slot keeps the expected Base Color/ORM/Normal/Emissive paths and links. Confine manifest folders to the export root.

**Reason:** testing only one set assigned to every material slot did not verify multi-material export wiring.

**Limits:** this checks paths, node wiring, and saved Blender data only; it does not evaluate rendered appearance, Godot/Unity behavior, or export profile quality. Blender 5.2.2 smoke passed locally.

**Evidence:** `scripts/qa/blender_multiset_smoke.sh target/debug/kalium` builds a saved two-Texture-Set project, exports 22 maps, and confirms distinct Leather/Metal paths after Blender save/reopen.


## D-026 — Verify recovery across a killed process

**Decision:** exercise journal recovery through child processes using the app's headless egui path. The writer commits a UV stroke, signals after the synchronous append has completed, and is killed without running app cleanup. A fresh process restores the journal after a torn final frame is appended.

**Reason:** in-process reopen tests do not establish behavior across process termination or prove that the durable append is visible to a fresh startup.

**Limits:** this validates journal durability and startup recovery without a display; it is not an OS power-loss test or a desktop recovery-dialog acceptance test.

**Evidence:** `process_kill_then_relaunch_restores_durable_prefix_after_torn_tail` kills a helper test process, then verifies byte-exact channel restoration, document validation, unsaved state, baseline hash preservation, and torn-tail truncation.

## D-027 — Step exports through the editor frame loop

**Decision:** keep the document owned by the UI thread and advance active/all-set exports by one logical PNG per frame. The job snapshots set identity/name/size only; cancellation leaves completed files as partial output and reports that state.

**Reason:** cloning a large document for a worker would duplicate decoded texture storage. Frame-stepping makes progress visible and provides cancellation without copying document state.

**Limits:** compositing/encoding one map can still block a frame; desktop responsiveness is unverified. Cancellation does not roll back files already written.

**Evidence:** active/all-set jobs match synchronous export bytes (including padded DirectX maps); cancellation produces no further maps; app tests verify progress and partial-output reporting.

## D-028 — Make multi-record imported texture recovery crash-atomic

**Decision:** represent an imported channel as one Begin/Commit transaction across synced journal records. Recovery replays only through the last committed transaction boundary and physically truncates any incomplete final transaction before future appends. Existing marker-free journal records retain their previous replay behavior.

**Reason:** each journal append is durable independently, so a process kill between row strips otherwise leaves a valid checksummed prefix that could be mistaken for a complete import.

**Limits:** the transaction type covers imported channel installs; generated layers and history snapshots use their own related transaction envelopes. A process kill is tested for imports and an interrupted snapshot is tested through journal truncation, but power-loss/storage-device behavior is not.

**Evidence:** transaction order/matching validation, malformed marker rejection, incomplete suffix truncation, append sequence continuation, and a child-process kill/relaunch test that restores an earlier paint but no partial imported layer.

## D-029 — Journal supported Undo/Redo before mutating history

**Decision:** validate a bounded forward replay plan from the pending history entry, stream events into journal batches, append and sync them first, then apply Undo/Redo. Retained layer payloads are copied into one 256-KiB snapshot chunk at a time; batches move event payloads without cloning. If the event cannot represent the full state transition, disable journal recovery before changing the document. Reinsert previously allocated layer IDs with a dedicated event that preserves the monotonic next-ID counter.

**Reason:** mutating history before journaling loses crash recovery, while journaling after mutation can leave the recovery stream behind the live document if the append fails.

**Limits:** oversized or unencodable layer snapshot sequences still pause recovery, and the 256-MiB journal cap can reject large history payloads. A 1K multi-channel test verifies batches span multiple records; peak memory and UI latency are not instrumented. Event appends still synchronously flush on the UI thread.

**Evidence:** planner tests cover pixel/mask directionality, absent-channel removal/recreation, flat snapshot row splitting, properties, reorder and stack operations; visitor output matches the collector wrapper and stops on sink failure. Recovery tests validate full-layer snapshot replay and transaction truncation. App tests reopen and restore paint, blank-layer, imported-layer and AO-layer Undo→Redo, removed-layer Undo, a 1K multi-channel snapshot across multiple records, and incomplete snapshot truncation; recovery remains active for empty Undo/Redo.

## D-031 — Recover mesh replacement as one bounded staged transaction

**Decision:** journal the normalized, serde-serialized `Mesh` and its material bindings in a Begin/Chunk/Commit transaction. Hash the exact serialized bytes with SHA-256, cap them at 32 MiB, and emit 4-MiB chunks. Replay stages bytes away from the live document, then checks length, digest, mesh validity and binding targets before assigning mesh and bindings together. An incomplete final transaction is truncated from its Begin record. Mesh import remains usable when journal recording exceeds limits, but recovery is disabled before the import mutates the document.

**Reason:** mesh replacement changes global material-slot semantics, so replaying slot bindings against the old baseline mesh can corrupt recovery state. A staged mesh plus bindings keeps each replacement atomic while preserving the existing project document and Texture Set contents.

**Limits:** mesh import Undo/Redo remains a separate history feature. The 32-MiB recovery cap may exclude valid larger imported meshes; in that case the import proceeds but cannot be crash-recovered until saved. Journal writes remain synchronous on the UI thread.

**Evidence:** S5-020 implementation is integrated in `src/recovery.rs` and `src/app.rs`. Codec and replayer tests cover complete multi-record restore, hash/mesh/binding rejection without mutation, malformed IDs/offsets, and incomplete-tail truncation. App Save→Import→reopen→Restore verifies exact mesh bytes, bindings, preserved inactive-set pixels, and a subsequent project save/reload. A 5-MiB unknown mesh metadata field verifies multi-chunk recovery and unknown-field preservation. A 32-MiB serialized mesh is accepted at the cap, one byte more is rejected, and an over-cap app import verifies recovery is disabled while import succeeds and reopening does not offer partial recovery. A child process killed after the first acknowledged async mesh chunk verifies that incomplete replacement is discarded while prior committed edits restore. Full worktree suite passes 293 tests; desktop recovery prompt acceptance remains pending.

## D-032 — Make Godot 4 export conventions explicit in a named profile

**Decision:** add a `Godot4Orm` export profile for stepped UI export and headless active/all-set export. It pins OpenGL (+Y) tangent normals, keeps the exact R=occlusion/G=roughness/B=metallic packed map, and writes a per-set JSON sidecar listing material type, map paths, channel layout, texture-set ID/name, and resolution. The existing generic profile and output maps remain unchanged. Do not claim Godot runtime acceptance unless a Godot editor import test is run.

**Reason:** Godot documents `ORMMaterial3D` as its packed occlusion/roughness/metallic path and requires OpenGL-style (+Y) normal maps. The exported artifacts need to carry those conventions with each Texture Set so they cannot be inferred incorrectly from a filename alone.

**Limits:** Godot is not installed in this environment, so generated profile artifacts are verified by automated pixel/JSON assertions only. The sidecar describes manual import/material setup; it is not a generated `.tres` resource. Unity and generic glTF profiles are tracked separately.

**Evidence:** [Godot 4.4 Standard Material 3D and ORM Material 3D](https://docs.godotengine.org/en/4.4/tutorials/3d/standard_material_3d.html) documents ORM material use and the OpenGL normal convention. Automated exact-channel, normal-orientation, per-set metadata, stepped export, and CLI artifact tests pass; editor import/render acceptance remains pending.

## D-033 — Describe the Unity metallic workflow without assuming a render pipeline

**Decision:** add a Unity export profile that selects the existing Metallic/Smoothness PNG and describes red=metallic, alpha=1−roughness, and OpenGL (+Y) normals. Write a per-set manifest that identifies the normal-map import type and suggests using alpha as smoothness. The profile makes no claim that one serialized material resource works across Built-in, URP, and HDRP.

**Reason:** Unity documents the Built-in Standard metallic workflow with metallic in red and smoothness in alpha; current Unity 6 docs identify Unity normal maps as Y+. The explicit sidecar makes the settings visible while avoiding a pipeline-specific material artifact.

**Limits:** no Unity Editor instance with the Pipeline package is connected, and no Unity editor import/reopen test was run. File assertions do not verify how a specific render pipeline renders the material.

**Evidence:** [Unity Standard Shader metallic map](https://docs.unity3d.com/ja/2022.1/Manual/StandardShaderMaterialParameterMetallic.html) and [Unity 6 normal maps](https://docs.unity3d.com/6000.1/Documentation/Manual/StandardShaderMaterialParameterNormalMap.html). Automated packed-channel, manifest, and CLI artifact assertions cover the exported files.

## D-034 — Describe generic glTF 2.0 metallic-roughness exports

**Decision:** add a `GltfMetallicRoughness` profile for active/all-set stepped and CLI exports. The per-set sidecar points `metallicRoughnessTexture` and `occlusionTexture` to the packed ORM PNG, describes G=roughness/B=metallic and R=occlusion, uses the Base Color+Opacity PNG as `baseColorTexture`, identifies linear data textures and OpenGL (+Y) tangent normals, and lists the other exported maps. The profile remains a texture-assignment guide; it does not emit a glTF model or material resource.

**Reason:** glTF 2.0 defines green as roughness and blue as metallic for the metallic-roughness texture, red as the occlusion value, and permits a shared packed texture. Explicit mapping avoids applying Godot's ORM channel order to glTF material slots without checking the standard.

**Limits:** this environment has no downstream glTF viewer acceptance. Sidecar, packed pixels and CLI outputs are tested; model authoring, alpha-mode selection/cutoff, texture-coordinate metadata and rendered appearance remain outside this profile.

**Evidence:** [Khronos glTF 2.0 Specification, Metallic-Roughness Material and Occlusion Texture](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.pdf). Unit tests verify per-set JSON fields, shared ORM paths, exact packed bytes and profile-pinned OpenGL normals; CLI smoke verifies map signatures/dimensions and source-project immutability.

## D-035 — Journal active Texture Set resolution changes

**Decision:** record active-set resolution changes as `TextureSetResize { set_id, size }`. Replay requires the stable ID to match the active set and the resolution to be in the supported range, then calls the document's budget-checked nearest-neighbor resize implementation. Inactive sets keep their own resolution and pixels.

**Reason:** resolution changes are ordinary document edits. Disabling crash recovery for them created an avoidable gap even though the operation is deterministic and targets one stable Texture Set.

**Limits:** resize still clears Undo/Redo history, matching current app behavior. UI-thread journal writes remain synchronous, and desktop recovery-prompt acceptance is open.

**Evidence:** direct replay tests reject invalid IDs and sizes without mutation and compare every active-layer channel and mask to nearest-neighbor output. App Save→Resize→reopen→Restore verifies channel maps, masks, inactive-set preservation, journal availability and Save/reload.

## D-036 — Keep model strokes on their source UV charts

**Decision:** model raster hits retain the source triangle and perspective-correct barycentrics. When a stroke moves between triangles that share an exactly matching geometric edge, paint corresponding points on both triangles' UV edges and avoid interpolating directly across the atlas.

**Reason:** duplicated vertices at UV seams map one geometric edge to two distant atlas locations. Direct UV interpolation can paint unrelated texture space.

**Limits:** shared edges currently require exact position matches and ambiguous topology is rejected. Sparse pointer events can still skip over intermediate triangles; approximate vertex welding, imported seam acceptance and desktop behavior remain open.

**Evidence:** renderer ray-oracle tests cover source IDs and barycentrics through overlap and near-plane clipping. A synthetic egui drag over duplicated seam vertices paints both charts, leaves the atlas gap untouched, and passes exact Undo/Redo composite checks.

## D-037 — Swap mesh history states and journal before applying

**Decision:** retain the previous mesh and its material bindings in one bounded history entry. Undo/Redo serializes the destination mesh through the existing bounded mesh replacement transaction before swapping the live document state.

**Reason:** mesh import used to erase the entire Undo/Redo stack, and a journaled mesh change had no inverse history operation. Ownership swaps avoid retaining a second copy of both active and inactive states.

**Limits:** retained mesh memory counts toward the 96-MiB history cap. If the prior mesh cannot be retained, import proceeds without Undo and clears prior history so commands cannot time-travel across an unrecorded replacement. A destination above the 32-MiB recovery transaction cap disables journal recovery before applying that Undo/Redo. Durable writes still synchronously flush on the UI thread.

**Evidence:** history tests assert exact serialized mesh bytes and bindings across sequential replacement Undo/Redo and reject invalid targets without partial mutation. App tests exercise import→Undo→Redo→reopen→Restore and import→Undo→reopen→Restore with material bindings and independent Texture Sets.

## D-038 — Preserve absent-channel state in snapshots and history

**Decision:** snapshot capture reads channel maps without mutating the document, represents an absent channel as logical transparent pixels, and stores whether the map existed before the operation. Undo removes a map created by the edit; Redo recreates it. Masks remain directly captured because every layer owns a mask.

**Reason:** querying a channel for history must not itself create project content. Undo should restore the prior document structure as well as its visible pixels, and recovery needs an explicit remove event for that case.

**Limits:** full-image before/after snapshots still use substantial memory for Fill/Clear; optimizing those payloads and ensuring preflight before mutation remain open.

**Evidence:** tests verify read-only capture, absent-channel Undo removal, exact Redo recreation, recovery planning of `LayerChannelRemove`, plus Fill/Flood Fill no-op behavior for transparent targets on absent channels.

## D-039 — Serialize recovery journal writes on a bounded background writer

**Decision:** move the open `Journal` handle to one dedicated writer thread. The app submits already encoded, bounded event batches through a FIFO whose accepted queued-plus-active requests are bounded by item count and payload capacity. Queue admission is nonblocking. Every accepted append has a ticket that resolves only after the writer's existing flush and `sync_all` complete. Ordinary committed edits enqueue and surface worker errors on a later UI update; Undo/Redo retains its direction and waits asynchronously for every planned event ticket before changing document/history state. Save/open/discard lifecycle barriers drain and join the writer before closing, deleting or replacing the journal.

**Backpressure/failure:** if an event cannot be admitted or a write fails, the app fails recovery closed and writes the existing disabled sidecar before offering another recovery. It may still apply Undo/Redo after recovery has been safely disabled. Pending Undo/Redo blocks document input until its durability tickets resolve; the UI shows pending status while continuing to repaint.

**Reason:** `Journal::append` flushes and calls `sync_all`; app callers currently execute this on the UI thread. Large snapshots and mesh transactions can perform many such syncs in one interaction. One FIFO owner preserves sequence and transaction Begin/Chunk/Commit order while removing disk sync from the frame callback.

**Limits:** project archive save, mesh serialization, event encoding and snapshot planning remain synchronous. The app queue is capped at 256 MiB and 128 accepted records; transactions that exceed it fail recovery closed. Tests cover FIFO, ticket acknowledgment, queue pressure, failure propagation, lifecycle drain, and unchanged history while recovery writes are pending. Process-kill behavior during queued async mesh/snapshot writes and GUI responsiveness remain unverified.

## D-030 — Bake AO on a worker and install it as an editable layer

**Decision:** run deterministic ambient-occlusion baking on a cancellable worker that owns a mesh snapshot and bounded options. Install completed pixels as an editable Occlusion layer, journal its multi-record creation inside a generated-channel transaction, and expose row progress and cancellation in Texture Set Settings.

**Reason:** AO is a long-running mesh operation and should produce project content artists can edit and save, while interrupted journal batches must not restore partial pixels.

**Limits:** AO settings are now retained per Texture Set by S8-003. Starting a bake still clones the mesh on the UI thread. Journal event writes now run through the bounded background writer, but desktop responsiveness and interaction remain unverified. AO Undo/Redo now uses the bounded full-layer snapshot transaction and can still fail closed at journal limits.

**Evidence:** worker tests cover deterministic results, progress and cancellation. App tests verify editable-layer installation, exact Save→Restore pixels, and Undo recovery; transaction scanner tests reject incomplete or mismatched generated-layer commits.

## D-040 — Start procedural materials with a bounded typed graph

**Decision:** the first S9 graph contract is a typed UV source → seeded 2D value-noise node → Roughness output. Node and edge counts are bounded, node IDs must be unique, parameters must be finite and in range, edges must connect compatible node types, and cycles or malformed input are rejected. The evaluator is deterministic for a seed and UV sample.

**Reason:** this establishes real graph validation and evaluation semantics before layering in a generic node canvas, effects stack or smart-material system. It keeps the first procedural output constrained to a common scalar PBR channel.

**Limits:** this initial core module is not yet connected to document layers, the compositor, project persistence, Undo/Redo, recovery, or the UI. It is not an artist-facing feature and must remain marked incomplete until those paths and their end-to-end acceptance tests are integrated. Evaluation caching and invalidation must be designed before per-frame layer evaluation.

**Evidence:** `src/procedural.rs` tests deterministic repeatability, seed variation, generated grayscale pixels, malformed connections, cycle/duplicate-ID rejection, non-finite input and graph size limits.

## D-041 — Autosave unsaved sessions through a validated baseline and journal

**Decision:** keep a per-user autosave baseline outside `.kalium` projects, and use the existing bounded, identity-and-digest-bound recovery journal to append supported edits for new/untitled sessions. Startup validates the baseline and offers the established Restore/Discard flow. Saving a real project replaces the session journal with a project-specific journal and removes the session baseline.

**Reason:** the current event journal already bounds payloads and writes through a durable FIFO worker. Reusing it avoids periodically deep-cloning large documents on the UI thread or consuming another full project copy in memory.

**Historical limits (superseded by D-077 for checkpointing):** this decision originally added journal-based recovery without periodic full-project snapshots. Unsupported edits still disable recovery fail-closed, and desktop prompt behavior remains unverified. A corrupt or unsafe baseline must produce a visible recovery error rather than being overwritten silently.

**Evidence:** `untitled_session_autosave_restores_edits_after_reopen` verifies that a new document's journal restores an edit while keeping Save As semantics.

## D-042 — Lock the per-user untitled autosave session

**Decision:** acquire a standard-library OS advisory file lock before reading, creating, or attaching to the per-user untitled-session baseline and hold the file handle for the session lifetime. A contender reports recovery unavailable and does not touch the session files. Release the lock after a successful Save As/Open transition; process termination releases it through the operating system.

**Reason:** a shared fixed baseline and journal cannot safely be recovered or removed by competing instances. The lock makes ownership exclusive without stale PID files, timestamps, or manual stale-lock cleanup.

**Limits:** this protects the shared untitled-session files only. It does not coordinate simultaneous edits to the same saved project/journal. Advisory lock semantics need cross-platform CI validation. Periodic full-project snapshots and desktop recovery-prompt acceptance remain open.

**Evidence:** app tests prove a contender fails before changing baseline/journal bytes, a later owner can restore the prior edit after lock release, and Save As preserves session data on write failure then transitions to the target project's UUID/digest journal on success. Pointer-driven modal tests verify Restore and Discard behavior.

## D-043 — Invalidate a journal when the disabled sidecar cannot be created

**Decision:** when fail-closed handling cannot create the `.disabled` sidecar, stop and drain the writer, overwrite the journal header and sync it before attempting file removal. A residual journal then fails header validation at startup and cannot present its earlier valid prefix as recoverable.

**Reason:** removing the file alone can fail too. A write to the existing journal may still succeed when creating a sibling sidecar fails, and invalidating the header gives startup an independent rejection signal.

**Limits:** if the filesystem rejects the sidecar write, journal write and journal deletion, no process can persist a tombstone; the app reports that it could not safely disable recovery. The current test forces sidecar failure while allowing journal invalidation. It does not simulate total filesystem write refusal.

**Evidence:** `failed_disabled_marker_invalidates_residual_journal_before_relaunch` leaves the invalidated journal file present, removes the injected sidecar obstruction, and verifies a new app reports a rejected journal without offering Restore.

## D-044 — Give each saved project one recovery owner

**Decision:** use a persistent OS advisory lock keyed by the canonical project path. Native Open acquires it before loading the project archive or journal, then holds it through recovery and unsaved-change dialogs. Save As acquires the destination lock before writing, keeps source ownership on save failure, and transfers ownership only after writing the project and resetting its journal. Canceling Open drops its provisional lock.

**Reason:** journal handles cache their append sequence/offset, and project saves share a sibling `.tmp` path. Multiple app instances can otherwise interleave recovery records or replace each other's temporary project archives.

**Limits:** hard-link aliases are not collapsed to a shared file identity. If an app is opened through a noninteractive test path while another owner holds the lock, it reports recovery unavailable; the native Open action acquires before load and fails early. Advisory lock behavior still needs cross-platform CI and desktop acceptance.

**Evidence:** app tests show a competing owner leaves journal bytes unchanged, later Restore works after release, Save As to a locked target preserves target bytes and writes no temp archive, a retry obtains target ownership, and canceling a pending Open releases its lock.

## D-045 — Store AO bake options per Texture Set

**Decision:** persist ambient-occlusion sample count, ray distance and deterministic seed under a namespaced per-set manifest extension. Journal edits as a typed event addressed by stable Texture Set ID. Derive the accepted sample-count limit from the set resolution and global ray budget.

**Reason:** bake quality and scale belong to the material set, and project recovery must restore the same parameters used for a later rerun. The existing extension maps preserve unknown fields and avoid a manifest-version bump.

**Limits:** older builds may ignore this reserved extension; desktop interaction and bake-result appearance remain unverified. This stores bake parameters only; it does not add high-to-low workflows or persistence for generated mesh maps.

**Evidence:** recovery tests verify set isolation, bounded validation and event replay. Project tests round-trip distinct active/inactive settings while keeping manifest version 7.

## D-046 — Sync project archives before and after replacement

**Decision:** finish the sibling temporary ZIP, sync its file contents, replace the primary within the same directory, then sync the containing directory on Unix. Use POSIX rename on Unix, `ReplaceFileW` for an existing Windows target and `MoveFileExW` for a new Windows target. Return a distinct typed error if directory sync fails after replacement, and let the app finish Save/Save As identity and recovery-journal handoff while reporting that durability is uncertain.

**Reason:** a successful `write`/ZIP finish does not prove the archive contents or directory entry reached stable storage. A remove-then-rename fallback could leave the primary missing if replacement failed.

**Limits:** local failure-order/reopen tests run on Linux; the Windows FFI path is uncompiled in this environment. Directory sync is not provided through the standard library on Windows or other non-Unix targets. These operations do not prove survival through device, controller, filesystem or sudden-power failures.

**Evidence:** project tests inject temp creation, archive write/finalization, temp sync, replacement and directory-sync failures. They assert pre-replacement failures preserve primary bytes, valid temps remain recoverable after replacement failure, post-replacement sync failure is distinguishable and the new archive reopens, and successful replacement removes the temp. App save handling downcasts the post-replacement error and preserves committed project identity.

## D-047 — Keep material IDs as data maps

**Decision:** encode each mesh material slot as a 24-bit little-endian integer in RGB, with alpha 255 for covered texels and 0 for uncovered texels. Give the first triangle in mesh order ownership of overlapping UV samples, filter output by material-to-Texture-Set bindings, and write a neighboring JSON sidecar with the encoding and slot names. Keep this map separate from PBR layers and `.kalium` project data until a typed persisted baked-map contract exists.

**Reason:** storing numeric IDs in Base Color would subject the values to color compositing and sRGB handling, which can change their encoded indices. A separate data map preserves exact values and can later feed procedural generators.

**Limits:** this is currently a standalone CLI artifact. The project does not yet retain baked-map assets, provide GUI progress/cancel for ID baking, or consume IDs in graph evaluation. The material slot index is currently bounded by mesh validation to 256 slots.

**Evidence:** analytic tests cover two slots, transparent gaps, Texture Set filtering, deterministic overlap ownership, cancellation and memory/raster bounds. CLI tests compare exact PNG bytes, sidecar fields and source archive bytes; the release smoke checks image dimensions and metadata.

## D-048 — Feed persisted AO layers into Roughness graphs

**Decision:** extend the typed graph with an optional Occlusion input that references a stable layer ID within the same Texture Set and a scalar Multiply node that modulates seeded noise. Resolve the raw Occlusion red channel by matching texel coordinates. AO source visibility, opacity, mask and channel switch do not change this input. The procedural result is the destination layer's Roughness base; if that layer also contains authored Roughness pixels, their alpha overlays the generated value. Destination opacity, mask and blend mode then apply once. A missing source layer or Occlusion map resolves to neutral white, preserving the graph's seeded-noise output without silently retargeting another layer. Keep the AO map as normal editable layer content. Save the new graph schema in manifest v8 so older applications reject graphs they cannot evaluate. AO image edits must refresh both Occlusion and dependent Roughness regions in the material/viewport cache.

**Reason:** AO baking already creates editable, persisted, undoable Occlusion layers. Stable IDs let graphs consume those pixels without duplicating large maps or introducing a second asset lifecycle. Neutral fallback preserves usable documents when artists delete or replace a source.

**Limits:** the input is same-Texture-Set and uses raw texels, with no cross-set links, UV remapping, filtering controls or generic node editor. Layer-dependency cache invalidation and desktop interaction need acceptance evidence.

**Evidence:** graph validation/evaluation, full-vs-region compositor, project v8 save/load, app Undo/Redo/recovery, and AO-edit incremental-refresh tests cover the implemented slice.

## D-049 — Preflight Wavefront text before invoking the mesh parser

**Decision:** scan bounded OBJ and referenced MTL text before passing either stream to `tobj`. Cap individual line allocation, total input bytes, source attribute counts, face/line corner and triangulated-index expansion, object/group/material boundaries, material-library references, aggregate MTL material declarations, and unknown MTL parameter counts and value sizes. Seek the same open file handle back to the start for the established parser after preflight succeeds.

**Reason:** source-byte limits alone do not constrain parser-owned geometry growth. The importer previously applied vertex/index limits only after `tobj` returned fully assembled models, allowing small repeated-face inputs to allocate substantially more than the accepted mesh budget.

**Limits:** preflight is a conservative resource gate; `tobj` remains responsible for OBJ/MTL syntax and semantic validation. Lines over 1 MiB and assets with over 100,000 object/group/material boundaries are rejected. MTL unknown parameters are bounded to 64 unique keys per material, 4,096 and 16 MiB of worst-case key/value storage across the full OBJ import, 128 bytes per key and 4 KiB per value. This does not replace malformed-input fuzzing or process-level memory limits.

**Evidence:** tests exercise exact and over-limit attribute/face/line/index cases, line and boundary caps, MTL per-material and aggregate material/unknown-parameter caps, and repeated-index face/line imports that `Mesh::from_obj` rejects at the expansion gate before parser assembly.

## D-050 — Refresh uniform channel edits through bounded cache stripes

**Decision:** after a committed direct-channel Fill or Clear, preserve a clean material cache by recompositing the changed channel in bounded full-width row strips and patching those rows in place. Keep the model viewport marked dirty for a correct full-frame render. Mask fills, Occlusion edits with dependent Roughness graphs, dirty/missing caches and patch failures use the full rebuild path.

**Reason:** a uniform channel edit changes every texel of one output map, but rebuilding all channels and Texture Sets is unnecessary. Row strips cap temporary composite memory independently of texture resolution. Cache fallback must not be treated as another document edit or disable an otherwise valid recovery journal.

**Evidence:** app tests fill and clear Base Color with a clean cache, compare the updated material map to a fresh document composite, verify unrelated Roughness cache bytes remain unchanged, and compare the next viewport render byte-for-byte with a fresh full render. Existing mask and AO graph tests cover their separate invalidation paths.

## D-051 — Bound parsed glTF source bytes and document nodes

**Decision:** keep the opened-file metadata size check as a fast rejection, then parse through a reader limited to `MAX_GLTF_FILE_BYTES + 1` so a source that changes after metadata inspection cannot make the parser read without bound. Reject the parsed document when its node table exceeds 100,000 entries, independently of which nodes are reachable from the selected scene. Retain the separate total visit limit for repeated node references during traversal.

**Reason:** file metadata alone is not a hard read limit, and a scene traversal budget does not bound an unreachable node table. Both inputs consume parser memory before selected-scene mesh assembly.

**Limits:** the full node array is parsed before `document.nodes().count()` can reject it. The raw source byte cap therefore remains the primary parser-memory bound; this count cap additionally constrains later document processing and prevents unused nodes from bypassing the format-level budget. Full parser fuzzing and peak-memory profiling remain open.

**Evidence:** helper boundary tests accept exactly the byte cap and reject one byte over; an integration fixture rejects 100,001 unreachable nodes before buffer or material assembly.

## D-052 — Add deterministic analytic environments to the CPU reference renderer

**Decision:** provide built-in Studio, Warm and Cool analytic sky/horizon/ground presets plus a broad softbox lobe in the CPU reference shading path. Add AO-weighted diffuse environment response and roughness-dependent reflected radiance. Expose preset selection in Brush Studio and include it in the existing lighting cache key so changes force a full redraw. Keep solo-channel previews raw and load no image asset.

**Reason:** artists need controlled lighting changes while painting metal and roughness maps. Deterministic analytic presets add this preview control without downloaded/generated imagery or a GPU dependency.

**Limits:** this is a CPU preview approximation, not HDR image-based lighting or a production renderer. It has no user-authored environment assets, broad visual-reference corpus or desktop acceptance evidence.

**Evidence:** renderer tests cover preset differences, repeatability, roughness response, non-finite bounds, cache invalidation/full-render parity and raw solo maps; a synthetic egui interaction selects the Warm preset.

## D-053 — Keep material-ID maps as standalone, snapshot-based exports

**Decision:** expose material-ID baking as a non-destructive Texture Set export. Snapshot the active mesh, material bindings, Texture Set identity/name and resolution before launching the cancellable worker. Use the same metadata builder and staged PNG/JSON writer in CLI and editor. Publish both files only after both are staged; on ordinary publication errors, restore an existing pair. Cancellation and failure do not edit the project or create an output pair.

**Reason:** material-slot IDs describe imported mesh topology and slot order, while PBR layers and project recovery have a different storage contract. Keeping the current map standalone makes the output usable immediately without presenting it as an editable project asset. Sharing one schema prevents CLI/editor sidecar drift; snapshotting prevents a switched Texture Set from relabeling a result.

**Limits:** this is not project-persisted generated-map storage or a procedural graph input. Worker progress is indeterminate; tests are headless and do not establish desktop visual acceptance. Pair publication rolls back ordinary write/rename failures but cannot guarantee an all-or-nothing transaction across sudden power loss during two filesystem renames.

**Evidence:** app tests cover a synthetic Texture Set Settings click, exact decoded PNG parity with the CPU bake, sidecar IDs/names/dimensions/slot table, cancellation with no files, output failure with no document mutation, and CLI/editor writer tests verify replacement and rollback behavior.

## D-054 — Verify exported PBR map influence in Blender CPU Cycles

**Decision:** extend the existing Blender import smoke with a deterministic two-set fixture that assigns distinct Base Color, ORM and tangent-normal values. After save/reopen, render the imported Principled graph with fixed CPU Cycles settings and compare it against three counterfactual renders where Roughness, Metallic or Normal is replaced by a constant. Keep the existing emission/UV pass as a separate check for map sampling and material-slot assignment.

**Reason:** node links and image paths prove graph structure, but do not prove the exported scalar and normal maps participate in shading. Counterfactual renders provide behavior evidence for those channels without claiming broad visual-quality parity.

**Limits:** Blender 5.2.2 CPU Cycles and a small deterministic fixture only. The check does not validate Godot/Unity imports, broad material corpora, color-managed production renders, desktop Kalium shading, or artist visual quality.

**Evidence:** the single-set legacy invocation and `scripts/qa/blender_multiset_smoke.sh target/debug/kalium` both pass after `.blend` reopen. Two-set regional RGB MAE against constant-input controls is 0.180954 for Roughness, 0.143056 for Metallic and 0.550776 for Normal; fixture checks also verify ORM channels and normalized tangent-normal pixels.

## D-055 — Expose bounded high-to-low projection with an explicit shared-space contract

**Decision:** expose the existing CPU high-to-low world-normal projection through `bake-high-to-low`. Import the high mesh with the same bounded OBJ/glTF/GLB path used by the application, filter low-resolution destination texels through the selected Texture Set bindings, and write normal, coverage and metadata outputs as a staged bundle. The command requires both meshes to already share coordinates and records this assumption in help and metadata.

**Reason:** the tested nearest-hit projection primitive can provide useful map data for aligned meshes without hiding the coordinate-space constraint or implying a complete cage workflow.

**Limits:** no transforms/alignment, cage controls, dilation, persisted generated-map asset, generator graph input, GUI progress/cancel, or production bake workflow. Peak importer allocation and sudden-power-loss atomicity are not proven by this CLI slice.

**Evidence:** CLI tests compare the generated normal PNG with direct API output (693 covered texels in the analytic fixture), verify coverage/metadata outputs, reject nonfinite/invalid ray distances and memory-budget underflow, and confirm the source `.kalium` bytes are unchanged. Output publication stages the full bundle and rolls back ordinary rename failures.

## D-056 — Preserve glTF tangent frames for CPU reference normal mapping

**Decision:** store optional glTF tangent direction and handedness on each mesh vertex. Transform tangent XYZ with the node linear transform, orthogonalize it against inverse-transpose transformed normals, normalize it, and flip W for reflected transforms. Preserve the field in project/recovery mesh serialization and have the CPU reference renderer interpolate supplied tangents through clipping and perspective-correct rasterization. Keep the current UV-derivative frame when any vertex on a source triangle lacks tangent data.

**Reason:** valid glTF tangent bases define tangent-space normal interpretation; dropping them can change imported surface shading, especially after nonuniform transforms and reflections. Optional storage keeps old projects readable and preserves fallback behavior for OBJ and glTF assets without tangents.

**Limits:** this verifies the CPU reference path only. Native wgpu mesh shading, editor visual acceptance and a broader glTF corpus remain open. Invalid tangent frames from imported files and project archives are rejected.

**Evidence:** importer tests check nonuniform reflected transformation, invalid values/counts and legacy JSON; project save/reopen preserves a tangent; renderer tests verify direction response, mirrored handedness and fallback parity. The integrated all-target suite and strict Clippy pass.

The tangent storage matches the public [Khronos glTF 2.0 specification](https://github.com/KhronosGroup/glTF/blob/main/specification/2.0/Specification.adoc), which defines `TANGENT` as normalized XYZ plus a W handedness sign and computes the bitangent as `cross(N, T) * W`.

## D-057 — Add optional source-coordinate alignment to high-to-low baking

**Decision:** keep the existing viewport-normalized high-mesh import when no transform is supplied. When an affine JSON matrix is supplied, preserve OBJ source coordinates or glTF coordinates after node transforms, then map those coordinates into the low project's stored mesh space. The matrix contains 16 column-major values acting on column-vector positions. Transform positions, inverse-transpose normals and tangent frames; reverse winding and tangent handedness under reflection. Cap transformed coordinate magnitudes at 1,000,000 units and record the matrix and low-project max-distance units in metadata.

**Reason:** ordinary high-mesh imports independently normalize geometry, which removes source placement and scale needed to align a high mesh to a saved low mesh. An explicit opt-in transform makes this mapping possible without changing the established default CLI output.

**Limits:** an affine transform does not infer correspondence or create a projection cage. It has no interactive alignment controls, dilation, persisted bake asset, graph input, GUI progress/cancel or desktop visual acceptance. Source coordinates from OBJ and node-transformed glTF do not constitute a universal coordinate convention; the caller must provide the correct mapping.

**Evidence:** mesh tests show coordinate-preserving import retains an offset OBJ's source positions. CLI tests apply a translation from a source plane at z=10 to a low-project plane at z=1.1, compare output with the direct projection API (352 covered texels), verify metadata contains the matrix and project bytes remain unchanged, and exercise identity parity, rotation/nonuniform scale, inverse-transpose normals, reflections, malformed/oversized/singular transforms and coordinate bounds.


## D-058 — Count degenerate OBJ face expansion before parsing

**Decision:** during bounded OBJ preflight, count a one- or two-corner `f` record as one degenerate triangle (three indices), because the pinned `tobj` triangulator expands those records when triangulation is enabled. Count ordinary polygon faces as `(corners - 2) * 3`. Track `l` record corners against the face-corner cap without treating lines as triangulated face indices.

**Reason:** the post-parse mesh-index cap cannot prevent allocation if many short face records expand into triangles first. Preflight needs to model the parser's actual expansion.

**Evidence:** helper tests accept the exact index cap and reject repeated short faces over it; an import fixture rejects many one-corner faces before parser assembly. Line corners have an independent cap test.

## D-059 — Match duplicated seam vertices with bounded scale-relative tolerance

**Decision:** shared-triangle-edge detection compares each position axis within `(1e-4 + f32::EPSILON) * max(triangle extents)`, requires one-to-one corner matches, and rejects ambiguous near-coincident matches.

**Reason:** independently exported UV charts often duplicate geometric vertices with small float drift. Exact equality prevents stroke continuity across those seams, while unbounded proximity can connect unrelated surfaces.

**Evidence:** mesh tests cover exact and near-equal shared positions, disconnected drift, and ambiguity. A headless model stroke across slightly mismatched duplicated vertices paints both charts, leaves the atlas gap unchanged, and restores exact pixels through Undo/Redo. Desktop and broader imported-mesh acceptance remain open.

## D-060 — Flush per-user workspace layout through atomic replacement

**Decision:** serialize workspace state to a synced sibling temporary file and atomically replace the layout file in the same directory. Sync the containing directory on Unix. Before accepting normal window close, immediately persist the latest workspace state even if the autosave throttle has not elapsed.

**Reason:** layout is a per-user preference outside `.kalium`; a recent dock change must not be lost if the application closes before the next periodic save, and replacement must not delete the prior valid workspace first.

**Evidence:** tests cover successful replacement, failure preservation, and a close request immediately after workspace changes. Windows native replacement is implemented but not compiled or runtime-tested in this environment.


## D-061 — Scope the current ZIP extraction advisories and keep a short expiry

**Decision:** keep the current `zip` dependency temporarily while Kalium's `.kalium` loader uses only named `ZipArchive::by_name` reads with decompressed-byte limits and the writer creates archives; no code calls `ZipArchive::extract` or `ZipStreamReader::extract`. Add exact, expiring cargo-deny exceptions for RUSTSEC-2026-0367, -0368 and -0369, limited to Kalium's direct dependency, and run the advisory check in CI. Reassess after the patched `zip >=9.0.0` source and lockfile can be resolved and tested locally.

**Reason:** the RustSec advisories describe extraction behavior, which the current project reader does not invoke. This avoids claiming the dependency is patched while preserving the bounded reader path and tracking the advisories explicitly. The offline registry has only `zip 0.6.6`; `cargo update -p zip --precise 9.0.0 --offline` cannot resolve the current `^0.6` requirement and no `zip >=9` source is cached.

**Evidence:** `cargo audit --no-fetch --deny warnings` reported the three extraction advisories. The project reader calls `ZipArchive::by_name` and bounds entry bytes; a source search found no extraction API call. `cargo audit --no-fetch --ignore RUSTSEC-2026-0367 --ignore RUSTSEC-2026-0368 --ignore RUSTSEC-2026-0369` completes with only the two separately reported transitive unmaintained warnings (`paste`, `ttf-parser`). See the [RustSec advisories](https://rustsec.org/advisories/RUSTSEC-2026-0367.html), [0368](https://rustsec.org/advisories/RUSTSEC-2026-0368.html) and [0369](https://rustsec.org/advisories/RUSTSEC-2026-0369.html).


## D-062 — Navigate the UV atlas while preserving screen-to-UV mapping

**Decision:** keep fit-to-atlas as the initial view, zoom around the pointer with a bounded scale (1×–16×), pan with the secondary drag while clamping the atlas so it remains recoverable, and reset with double-click. Convert paint coordinates through the transformed image rectangle.

**Reason:** a fit-only atlas cannot support detailed edits on small islands at working resolutions. Pointer-anchored zoom avoids moving the target away from the cursor, while clamped pan prevents losing the atlas.

**Evidence:** a synthetic egui test drives wheel zoom and secondary drag through view_pane, checks the UV coordinate under the zoom anchor, paints at a target texel, and verifies exact .kalium save/reopen pixels. Native gestures and high-DPI behavior remain unverified.

## D-063 — Bound resize transient allocation, not only final texture storage

**Decision:** before mutating any image, calculate the maximum memory peak across the actual replacement order: inactive-set bytes, outputs already replaced, all remaining source buffers and the current destination allocation. Reject if any intermediate peak exceeds the 512-MiB decoded texture budget.

**Reason:** an in-place assignment allocates the new image before dropping the old image. A final-size-only budget misses this temporary overlap and can overrun the cap during up- or down-resize.

**Evidence:** helper tests cover exact limit, one byte over, downsize source/destination overlap, and inactive-set bytes. No process-RSS benchmark was available.

## D-064 — Fail closed on glTF data the mesh schema cannot preserve (partly superseded)

**Decision:** reject selected-scene glTF primitives with UV sets beyond the currently supported range, skinning attributes or morph targets, and reject nodes/meshes carrying skin or morph weights. Return an actionable static-mesh import error until Kalium can preserve that data. D-066 adds preservation for TEXCOORD_0 through TEXCOORD_7.

**Reason:** importing only position/normal/tangent/UV0 would silently discard material-relevant UVs or deformation data and make a project round trip destructive.

**Evidence:** regression fixtures verify TEXCOORD_8, a primitive morph target and a skinned node each fail with a clear error. Full suite passed 454 tests at the preceding checkpoint; skin/morph preservation and UI diagnostics remain open.

## D-065 — Preflight project ZIP directories before archive construction

**Decision:** before calling `ZipArchive::new`, parse the bounded EOCD/ZIP64 footer, cap entries at the maximum representable project contents, cap central-directory bytes at 16 MiB, and cap each entry name, extra field and comment. Validate every central-directory record boundary before returning to the ZIP library.

**Reason:** `zip 0.6.6` reserves its entry vector and name map using the archive-declared count and parses all names/extras before returning. A post-construction count check cannot bound that allocation. The preflight keeps those allocations bounded for untrusted project files.

**Evidence:** classic ZIP exact-limit and over-limit count fixtures, ZIP64 bounded and over-limit fixtures, oversized central-directory/name fixtures, and a loader-level over-limit test pass. Full suite passes 464 tests; patched dependency migration remains pending.

## D-066 — Preserve bounded glTF UV sets in mesh data

**Decision:** retain TEXCOORD_1 through TEXCOORD_7 in an index-keyed `Mesh::extra_uv_sets` map, with one optional coordinate per mesh vertex. Keep TEXCOORD_0 in `Vertex::uv` as the active painting and picking coordinate. Reject set indices above 7 and cap the total supported set count at eight. Default missing serialized maps to empty so older `.kalium` mesh records remain readable.

**Reason:** rejecting all additional UVs avoids data loss but blocks production assets that use lightmap or alternate texture coordinates. Persisting the extra sets permits lawful import and round trips while preserving the existing UV0 painting behavior; the per-set vector alignment handles primitives that omit a nonzero set.

**Evidence:** glTF fixtures verify V-coordinate convention, TEXCOORD_1 values, alignment across primitives where one lacks that set, TEXCOORD_8 rejection, mesh JSON migration and `.kalium` save/reopen. Mesh replacement history accounting includes the extra UV allocation. Painting/export still use UV0 and need explicit downstream validation before claiming multi-UV authoring.

## D-067 — Use a bounded per-set layer tree for groups and fill layers

**Decision:** evolve each Texture Set from a flat paint-layer vector to an ordered, recursive layer tree with stable per-set node IDs and a maximum depth of 32 / 4096 nodes. Node kinds are Paint, Fill and Group. Paint leaves retain channel images; Fill nodes store explicit constant per-channel values (procedural inputs remain typed graph data); Groups own ordered children. Common node properties include name, visibility, opacity, blend mode, channel switches and mask. Painting targets an explicitly selected paint/fill leaf; selecting a group edits group properties and never redirects a stroke. The compositor carries channel values plus coverage through isolated nodes, evaluates children bottom-to-top, then applies each group mask/opacity once before blending into its parent; the channel background is introduced only at the root. Texture Sets remain independent.

**Frozen core API:** `LayerTree { roots: Vec<LayerNode>, selected_node_id: u32, next_node_id: u32 }`, where root/child vectors are bottom-to-top. `LayerNode` is a tagged enum `Paint(Layer) | Fill(FillLayer) | Group(LayerGroup)`. `Paint` reuses the current pixel-backed `Layer`; `FillLayer` has stable `id`, common layer properties, mask/channel switches/extensions, and `values: HashMap<Channel, [u8; 4]>`; `LayerGroup` has stable `id`, common group properties, mask/channel switches/extensions, and `children: Vec<LayerNode>`. IDs are unique per set and `selected_node_id` resolves only to Paint or Fill for painting. Groups never become paint targets. Read-only tree operations provide validated depth-first lookup, parent/path lookup and ordered paint-leaf iteration; mutations remain explicit document commands so history and recovery can journal them.

**Migration:** write a new `.kalium` manifest version for the tree. Convert v1–v8 flat layers one-for-one to Paint leaves, preserving order, selection, IDs, masks, pixels, properties and unknown extension fields. Continue loading old journal events with stable-ID adapters. Enforce depth/node/image-byte limits before recursive allocation and retain the current decoded-image budget. Group deletion and recovery must stream retained subtree data through bounded transactions.

**Reason:** wrapping the current flat opaque compositor would lose transparent group coverage or apply child alpha twice. A bounded tree plus an explicit coverage contract supports nested group opacity/masks, channel-disabled children and parametric fills without changing current paint semantics implicitly.

**Evidence required:** numeric composites for transparent and nested groups, masks/opacity applied once, channel-disabled nodes, reorder, each current blend mode, normal/RNM behavior and fill values; v1–v8 migration and unknown-field preservation; history/recovery of tree operations and retained subtrees; Layers-pane add/reorder/selection/property interactions; end-to-end save/reopen artist workflow. This decision freezes the interface only; no group/fill behavior is claimed implemented.

## D-068 — Keep a bounded user-local activity log separate from project recovery

**Decision:** add a versioned JSON Activity Log under the per-user Kalium configuration directory, separate from `.kalium` and the crash-recovery journal. The core module exposes `load`, `open_or_default`, newest-first entry access, `record(level, category, message)` and `clear`. Entries carry a Unix-millisecond timestamp, severity and bounded UTF-8 category/message. Retain at most 256 entries, 48 category bytes, 768 message bytes and 256 KiB on disk. Reject malformed, oversized or unsupported-version files before unbounded parsing. Mutations update memory only after a synced sibling-temp write and atomic replacement; sync the parent directory on Unix. The GUI adds a dockable Activity pane and records operations only at successful commit boundaries, with warnings/errors recorded as such.

**Reason:** the replaceable status bar is useful for the latest action but cannot explain earlier work or survive restart. The recovery journal has different safety semantics and must not become a user-facing log.

**Evidence required:** bounded load/truncation/eviction; restart persistence; failed-write state and prior-file preservation; log clear; synthetic pane show/restore and rendered entries; verified-operation vs canceled/failed-action semantics; native desktop acceptance where available.

## D-069 — Treat paint-layer selection as persisted document state

**Decision:** when a user selects a different layer row, resolve the selection through its stable layer ID, finish any pending stroke, mark the project dirty, and append a `LayerSelection` recovery event. Selection does not enter Undo/Redo history; Save/Reopen and crash Restore preserve the selected layer independently of pixel edits.

**Reason:** the native manifest already stores the selected layer, and the recovery codec already supports stable-ID selection events. A selection-only change must not disappear on close or after a crash just because it did not alter pixels.

**Evidence:** a synthetic egui pointer click changes selection and dirty state, then a native save/load preserves it. A later selection change is restored from the saved-project journal. Layer drag/reorder selection tests continue to pass. Native desktop click behavior remains unverified.

## D-070 — Composite material Opacity through the CPU reference viewport

**Decision:** in the normal PBR viewport, sample the red channel of the bound material's Opacity map and composite covered surfaces front-to-back in linear light over the viewport gradient. Retain up to eight nearest depth-sorted fragments per pixel; deeper fragments are omitted. Keep picking tied to the nearest geometric surface even when its opacity is zero. Solo-channel display remains a raw, opaque view of the selected map. Any Opacity edit invalidates incremental rendering; other edits also fall back to a full render while the cached normal PBR frame contains visible transparency.

**Reason:** Opacity must affect the material preview without changing which surface receives paint. The current one-hit cache can update opaque views incrementally, but transparent output depends on rear surfaces and cannot be refreshed by shading only the nearest hit.

**Evidence:** overlapping-triangle tests cover zero/half/full opacity, rear visibility, depth picking, raw solo-channel display, deterministic output, the eight-fragment bound, cache invalidation and opaque-channel fallback. An app-level Opacity edit checks pick stability and full-render equality. The CPU rasterizer remains a reference implementation; desktop visuals and renderer behavior beyond the eight-layer cap remain unverified.

## D-071 — Make the recursive layer tree the document source of truth

**Decision:** active `Document` state and every inactive `TextureSet` own a `LayerTree`; do not maintain a parallel flat layer vector. Root and child order remain bottom-to-top. Stable IDs are unique within each set and all structural operations address `(set_id, node_id)` with expected parent and sibling index where needed. UI selection may identify Paint, Fill, or Group. Painting resolves only when the selected node is a Paint leaf; Fill and Group selection never redirects a brush stroke to a child. Fill values and common Paint/Fill/Group properties are edited through typed node APIs. Per-set validation enforces depth 32, node count 4096 and the existing 512-MiB decoded image/mask budget with checked accounting.

**Frozen Rust surface:** `Document.layer_tree` and `TextureSet.layer_tree` are the stored trees. `LayerTree::select(id)` accepts any existing node; `selected_paint_layer[_mut]()` returns a paint layer only for a selected Paint node; `paint_layer[_mut](id)` resolves stable IDs through nested groups. Structural operations are checked `insert(parent_id, index, node)`, `remove(id, selected_after_id)`, and `move_node(id, parent_id, index)`; they preflight cycles, depth, node-count, selection and ID allocation before mutation. Removing a selected node/subtree requires an explicit existing `selected_after_id` outside the removed subtree; moving a node preserves selection. Read-only traversal exposes all node IDs and bottom-to-top paint leaves. `LayerTree::composite` and `composite_region` implement the coverage contract below. Existing callers migrate to these APIs; no flat compatibility vector is retained.

**Compositing contract:** evaluate each node into a channel value plus coverage. Children composite bottom-to-top in an isolated group result; apply a Group's visibility, disabled-channel state, opacity and mask once when the group result is blended into its parent. Apply the document channel background only at the root. Preserve current leaf blend behavior, procedural Roughness inputs, and bounded region/full-frame parity. Full and region tree composites must be byte-identical for the requested rectangle.

**Migration sequence:** first move `Document` and `TextureSet` storage to LayerTree and adapt existing Paint-only operations without exposing Fill/Group creation. Next write manifest v9 recursive tagged nodes with explicit node/parent/child ordering, fill values, selection and stable IDs; convert v1–v8 flat layers to root Paint nodes and preserve unknown fields. Then add tree-aware Undo/Redo and bounded subtree recovery transactions. Enable group/fill UI only after save/load, history and recovery preserve nested data. Reject over-deep/over-count structures before recursive construction and preflight total decoded allocations before mutation.

**Recovery event names:** `LayerTreeNodePropertiesSet { set_id, node_id, name, visible, opacity, blend, disabled_channels, procedural_graph }`; `LayerTreeFillValueSet { set_id, node_id, channel, value: Option<[u8; 4]> }`; `LayerTreeNodeMove { set_id, node_id, from_parent_id, from_index, to_parent_id, to_index }`; and `LayerTreeNodeRemove { set_id, node_id, expected_parent_id, expected_index, selected_after_node_id }`. Reinsert/restore uses `LayerTreeSubtreeBegin/Chunk/Commit`, carrying transaction/set/root/parent/index/resulting-selection/node-count/encoded-length/SHA-256 metadata and ordered chunks no larger than 256 KiB raw. Retained subtree payloads are capped at 96 MiB, with existing journal record and total-file caps also enforced. Existing `Layer*` recovery events remain compatible with migrated root Paint nodes. Mask edits reuse compatible bounded mask events where possible; new mask events must address any node by stable ID.

**Evidence required:** compositor value/coverage oracles for nested groups, group opacity/mask exactly once, channel disabling, all current blend modes, normal/RNM, fill values and region parity; v1–v8 migrations and v9 save/load/save with unknown-field preservation; no-mutation tests for invalid/capped tree edits; nested Paint/Fill/Group Undo/Redo and crash Restore, including interrupted subtree transaction truncation; headless UI workflows followed by save/reopen/recovery. This decision changes the S4 API contract; it does not claim tree persistence or user-facing group/fill features already exist.

## D-072 — Duplicate layer-tree subtrees with fresh stable IDs

**Decision:** duplicate the selected Paint, Fill or Group node and its complete subtree as one undoable operation. Insert the copy after its source, select the copied root, and append the bounded ` copy` suffix to its name. Allocate fresh IDs for every copied node. Update procedural AO source IDs only when their target is also within the copied subtree; external stable-ID references continue to point at the original node. Reject limits before changing the document or history, including node/paint-leaf counts, history retention, decoded texture storage, aggregate unknown extension data, and stable-ID counter overflow. Recovery emits a bounded subtree transaction, and Restore reconstructs children from validated preorder data in reverse attachment order so Groups are complete before they move into their parent.

**Reason:** duplicating only Paint leaves blocks the required layer-stack workflow for Fill and Group authoring. Cloning a Group without rekeying references would create duplicate stable IDs or silently change the meaning of AO inputs.

**Evidence:** nested Group with Paint/Fill descendants retains content and masks, fresh IDs, internal AO remapping, external AO preservation, one-step Undo/Redo, no-mutation limit rejection, synthetic toolbar pointer clicks on Group and Fill nodes, and Save→reopen→Restore. Root-level Fill duplication and native desktop interaction remain to be verified.

## D-073 — Bound incremental viewport damage to multiple rectangles

**Decision:** represent a material-tile refresh as up to 64 screen-space damage rectangles. Mark visible hit pixels that sample changed material tiles, coalesce connected 16×16 screen cells, and shade only those hit pixels. Disconnected regions remain separate partial uploads. If bounded damage allocation fails or more than 64 components are needed, return `CacheInvalid` before changing the framebuffer so the app performs a full render. The app merges overlapping regions from multiple refreshes in one frame and falls back to a full render if its pending rectangle cap is exceeded.

**Reason:** one bounding rectangle for distant UV/material edits can upload large areas of untouched viewport pixels. A fixed rectangle cap bounds both renderer output and queued app-side texture uploads.

**Evidence:** disjoint visible UV islands return separate rectangles; tests verify changed-pixel coverage, byte-identical unrelated pixels, full-render parity, adjacent-cell coalescing and 65-component rejection with no framebuffer mutation. App tests cover overlapping rectangle merging and its 64-rectangle pending cap. The viewport remains the CPU reference implementation; desktop texture upload behavior and clustered/dispersed performance measurements remain unverified.

## D-074 — Isolate a selected layer as a transient preview

**Decision:** selected-layer solo is display state owned by the app, not document data. Compose only the selected node while retaining its ancestor Group path and each ancestor's authored visibility, channel enablement, mask, opacity and blend behavior. Soloing a Group keeps its entire subtree. Use the isolated composite for the active Texture Set's material preview and bounded edit refreshes; do not alter node flags, history, recovery records or project dirty state when toggling solo.

**Reason:** temporarily hiding siblings by changing saved visibility can corrupt project state or lose the artist's authored visibility. Filtering only at the selected node would also bypass nested Group compositing semantics.

**Evidence:** nested Paint/Fill/Group composition tests cover sibling exclusion, group channel disable, mask/opacity, and full/region parity. A synthetic Layers-pane pointer test checks isolated Fill and Group maps, unchanged visibility flags and project dirty state, and exact restoration when solo is turned off. Native desktop interaction and visual review remain open.

## D-075 — Keep journal append admission within scan limits

**Decision:** a journal must reject a new record before writing when its next sequence would exceed the scanner's one-million-record cap. Every acknowledged append must leave the journal valid and reopenable under the same byte, payload and record limits used by scanning.

**Reason:** a small valid payload could previously push an open writer past the scanner's record-count limit while remaining below the 256-MiB byte limit. That append could be acknowledged but the journal would be rejected on relaunch.

**Evidence:** a boundary test advances the writer's sequence to the first disallowed record, confirms append rejection without file mutation, then scans the journal successfully. The one-million-record physical boundary is not stress-written in tests.

## D-076 — Keep export resolution independent from Texture Set resolution

**Decision:** allow bounded export sizes without resizing project Texture Sets. Resample maps before packing, filter color channels in linear light, average Normal maps as vectors, and use the output resolution for UV coverage and padding.

**Reason:** users need output-size control while preserving editable source resolution and channel-specific sampling behavior.

**Evidence:** tests cover all-map output dimensions, linear color/scalar/normal filtering, output-resolution padding, unsupported sizes and document immutability. Native Texture Set size remains the default byte-preserving path.


## D-077 — Publish recovery checkpoints as immutable generations

**Status:** Accepted (headless implementation and fault-injection tests; desktop and cross-platform crash acceptance remain open).

Recovery compaction must not truncate a journal while its baseline digest still points at the old archive. The store therefore writes an immutable checkpoint and a matching empty journal whose header binds the checkpoint SHA-256, syncs both directory entries, then atomically replaces a small manifest containing the active generation and (when safe) the prior generation. Startup validates the manifest identity, checkpoint and journal before selecting active; a corrupt active pair can fall back to the prior pair. Unreferenced generation files are ignored. A 16 MiB journal threshold triggers compaction after draining the single writer and while no active stroke/history/recovery/bake/export operation is in flight.

Restore loads the selected checkpoint before replaying that generation's journal. Explicit Discard publishes the saved primary as a clean generation with no previous fallback, preventing an older unsaved edit from reappearing. Save As switches to a clean generation for the destination identity. Failure injection before manifest replacement leaves the old pointer active; a failure reported after replacement leaves the new pointer active and discoverable.

**Tradeoffs and remaining work:** the worker drains the journal and owns the real Document, while the UI renders progress and editing/file actions pause; this avoids blocking the UI on queued durable appends and avoids a deep clone; this avoids a deep clone and prevents edits racing an empty journal. The gated progress test confirms close requests are canceled while compaction runs, and Drop joins the worker before releasing project state. Measure large-project serialization and drain latency and add app-level worker failure/restart coverage before claiming production responsiveness. After a successful manifest switch, best-effort cleanup retains only active and previous generation pairs and reclaims older/orphaned pairs; cleanup failures may leave removable files until a later successful publication and are exposed as a non-fatal recovery status warning. Directory durability and replacement semantics still require Windows/macOS and device-level crash testing. This protocol protects application-visible publication ordering; it does not claim hardware power-loss guarantees.


## D-078 — Migrate native project archives to patched ZIP 9

**Status:** Implemented; full suite, license and release validation pending. This decision supersedes the temporary exception policy in D-061.

**Decision:** use `zip 9.0.5` with default features disabled and the existing `deflate` feature. Use `SimpleFileOptions` for archive writing. Keep the project-specific classic ZIP/ZIP64 directory preflight and exact named-entry reads. Remove the three scoped RustSec ZIP exceptions.

**Reason:** RustSec marks `zip 0.6.6` affected by extraction-path advisories; relying on Kalium not calling extraction APIs left a vulnerable direct dependency in the lockfile. The host Rust 1.97 toolchain meets the candidate crate's Rust 1.88 minimum.

**Evidence so far:** crates.io resolved and downloaded `zip 9.0.5`; `cargo tree --locked -i zip` shows only `zip 9.0.5` as Kalium's direct archive crate. `cargo audit --deny warnings` reports no ZIP vulnerability, though it exits 1 on five INFO unmaintained notices. Project migration/corruption tests and the all-target suite must pass before verification.

## D-079 — Keep Recent Projects outside documents and reuse guarded Open

**Decision:** store up to 12 canonical paths in version-1 user-local JSON, with
4096 UTF-8 bytes/path and 32 KiB total. Promote only after an accepted primary
open or successful archive replacement, even when later recovery setup fails.
Recovery sources and canceled/failed opens are excluded. File picker and Recent
menu share the same path-based request, provisional lock and Save/Discard/Cancel
flow. Missing rows are disabled but removable; activation rechecks existence.

Persist through synced same-directory temporary replacement before updating the
in-memory list. Bound backup copying as well as reads, preserve supported-version
unknown fields, collapse canonical aliases, and reject non-UTF-8/NUL paths rather
than silently changing them. A history write error is a nonfatal warning that
cannot undo or misreport an already successful project save/open.

**Reason:** a separate direct load from a Recent row could bypass unsaved changes,
project ownership or recovery prompts. Recording a picked path before acceptance
would promote canceled or damaged recovery sources. UI preferences must remain
outside `.kalium` artistic content.

**Evidence:** focused storage and app tests exercise accepted saves/opens, bounded
and malformed storage, alias preservation, failed replacement, missing-file
removal, unsaved choices, recovery exclusions/guards and lock contention. See the
current QA matrix and S12-011 for integrated checks and runtime acceptance.

## D-080 — Stable legacy recovery identity and rejected-data preservation

Versions 1–5 derive an RFC-variant UUIDv8 from a domain-separated, bounded
SHA-256 stream over the already-open archive descriptor. Identical bytes retain
identity across reloads/copies; v6+ authored IDs remain unchanged. Saving as v9
persists the derived ID. No source archive is modified during loading.

Recovery identity/digest checks remain authoritative. Older journals written
with random legacy IDs are rejected rather than guessed or rewritten. Rejected
journals, checkpoints and autosave baselines remain intact across ordinary
Save, Save As and document Discard. Sequence validation, failed Restore, and
failed baseline reload must mark the source protected before disabling recovery.
Unsupported-edit invalidation remains a separate policy for preventing replay of
an incomplete supported-edit history. Use Save As to a different path to start
fresh recovery when the current source has rejected data.

See `legacy-project-identity-contract.md` and S5-038 for acceptance evidence and
the remaining legacy-journal migration limitation.

## D-081 — Preserve intentional empty workspaces and modal keyboard ownership

An empty dock is a valid per-user preference: every pane can be closed, the
Window menu remains available, and reopening a pane never changes project data.
Load it as empty rather than silently restoring defaults. Workspace reads stop
at the existing 1-MiB limit plus one sentinel byte; malformed/oversized layouts
fall back. New JSON carries `kalium_workspace_version: 1`; legacy unmarked JSON
remains accepted, while unsupported explicit versions fall back.

Explicit menu shortcut handlers return while unsaved-change or recovery choices
are pending. A modal blocks background pointer input through egui, but manually
polled keyboard shortcuts require this additional guard. Save/Open/history/tool
shortcuts must not bypass the pending choice. The modal's own Save/Discard/Cancel
and Restore/Discard buttons keep their existing paths.

## D-082 — Canonical 2026 visual workspace and viewport-owned controls

Use the brief's 1920×1080 Painter 2026 screenshot as the canonical visual
composition. The earlier supplied Kalium split-view screenshot is a baseline,
not the final geometry target. First launch/default reset uses a dominant 3D
canvas with UV as a sibling tab, narrow Assets at left, Texture Sets then
Layers/Texture Settings then contextual Properties at right, and 38 px tool and
dock-shortcut rails. Saved valid user workspaces retain their own arrangement.

Brush parameters belong to the viewport stripe, with a small editable number
above a thin track; they retain their original fields and inclusive ranges.
The right rail focuses/restores existing panes through the workspace API only.
Properties uses a single contextual tab heading and paired near-square previews;
layer rows use square thumbnails and separate blend/opacity lines.

Display the model over the available dock area while uniformly scaling its
reference buffer within the existing 1100×900 limits. GPU preview and CPU picking
receive the same bounded dimensions. Picking continues to normalize through the
displayed image rectangle. This is display geometry, not native GPU painting or
new PBR functionality. Keep the CPU reference and all project content semantics.

No Adobe preset/icon/branding assets or image-generation tools are used. Small
monochrome glyphs and transparency checkers are independently drawn UI geometry.
Screenshot acceptance remains required independently of tests/builds.

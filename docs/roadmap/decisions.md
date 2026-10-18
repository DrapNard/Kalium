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
**Limits:** image events cover committed strokes and snapshot-based image edits. Layer property and reorder events are now supported; create/remove/duplicate, texture-set, mesh, binding and undo/redo events remain unsupported and disable recovery until a new save. A single JSON event batch may not exceed the 8 MiB frame payload; history extraction also limits raw patch bytes to 1 MiB. App appends synchronously on the UI thread; desktop modal and process-crash validation remain open.
**Evidence:** exact active/inactive-set channel and mask tests round-trip through the bounded codec; malformed bounds and over-cap JSON fail without applying a patch. Headless app tests cover save→append→reopen→Restore for image and supported layer edits, Discard, unsupported mutation fail-closed behavior, and truncated-tail prefix recovery with append continuation.

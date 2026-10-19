# Compatibility status (2026-10-19)

**Legend:** ✅ Core behavior has automated evidence; 🟡 Partial or visually unverified; ❌ Missing. The current Linux worktree passes the local checks in `docs/qa/test-matrix.md`; GUI and downstream applications have not been visually tested.

| Feature/format | Status | Reality |
|---|---|---|
| Kalium project `.kalium` | ✅ | Editable ZIP/JSON/PNG format, versions 1–9; v9 stores recursive Paint/Fill/Group trees and v1–v8 flat layers migrate to Paint roots. Loading bounds archive, manifest, tree depth/node count, aggregate decoded images and retained extension metadata before constructing project images/metadata. Unknown project, set, tree and node fields are retained. Validated sibling-temp recovery, manual checkpoints, project-identity-bound recovery journals, threshold-triggered generation checkpoints and durable replacement sync the completed temp before replacement and the parent directory on Unix. Recovery generations pair an immutable checkpoint with its digest-bound journal and switch through a synced manifest; the journal drain and checkpoint serialization run on a worker while the UI remains responsive with editing paused; desktop crash/power-loss acceptance remains open. V8 adds AO-driven Roughness graph inputs referencing stable same-set layer IDs. Windows uses native replacement APIs; directory-entry durability is not verified here. |
| OBJ import | 🟡 | Triangulated geometry and `.mtl` slot names with per-triangle IDs; source-byte and material-path limits; limited flattened mesh representation |
| glTF/GLB import | 🟡 | Triangle primitives, position/normal/UV0/material slots; complete UV0 is required with a mesh/primitive-specific error when missing; node transforms are baked into geometry and optional tangents are transformed/persisted for CPU reference normal mapping; source/buffer limits; unused images skipped. TEXCOORD_1–7 are retained as per-vertex data across mesh/project round trips; UV0 remains the active paint channel, and UV8+ rejects. Skin and morph data remain unsupported and fail closed. Desktop import/shading acceptance remains unverified. |
| BaseColor / Roughness / Metallic / Normal / Height / Emissive / Opacity / AO | 🟡 | Eight editable raster channels. Base Color and Emissive brush source-over and layer compositing use linear-light RGB; Normal stamps normalize signed-vector interpolation, normal layers interpolate unit vectors, and Detail Normal uses reoriented normal mapping. Numeric headless tests cover these paths. Scalar channels blend normalized data values directly. Export alpha semantics and GUI visual behavior remain incomplete or unverified. |
| Layer opacity/visibility/masks | 🟡 | Normal, Multiply, Add and Detail Normal modes are implemented; Detail Normal applies RNM to the Normal channel and Normal semantics to other channels. Recursive Paint/Fill/Group trees support per-node masks, visibility, opacity and channel disabling; bounded undo/redo and recovery preserve stable-ID edits and subtree moves. Selected-layer solo is a transient preview that preserves ancestor Group semantics without changing project visibility. Headless egui acceptance covers Add Group/Fill, pointer-driven Paint reparenting into/out of a Group, nested edits, solo preview, Save/reopen and crash Restore. Broader nested-tree gestures and desktop compositing review remain unverified. |
| 3D paint strokes | 🟡 | CPU UV hit buffer, tested stroke interpolation and 64×64 tile-sparse stroke undo; a 4K small-dab fixture verifies capture stays under 40 KiB. GUI visual behavior and seamless projection remain unverified |
| GPU rendering | 🟡 | egui UI uses wgpu; 3D mesh preview is the CPU reference rasterizer, native GPU painting is future work |
| Physically based preview | 🟡 | Deterministic CPU Cook–Torrance GGX reference with metallic/roughness, tangent normals, emissive, ambient fill, and Studio/Warm/Cool analytic environments. No HDR IBL or desktop visual acceptance. |
| Brush pressure/tilt/rotation | ❌ | Mouse only at the moment |
| Multiple editable Texture Sets | ✅ | Independent editable stacks, material bindings, tested headless paint/save/reopen/export workflow; GUI workflow remains unverified |
| UDIM | ❌ | Single 0–1 UV canvas per set, no UDIM tile addressing |
| Mesh-map baking | 🟡 | Headless same-mesh world-normal and normalized-position PNG bake with UV coverage metadata; deterministic material-ID bake writes a 24-bit little-endian slot-index PNG with a shared JSON sidecar from both CLI and editor, using a cancellable worker; `bake-high-to-low` projects world normals to low-project coordinates with an optional bounded affine high-mesh transform and writes standalone normal/coverage/metadata outputs; same-mesh AO has a bounded deterministic CPU CLI bake (`bake-ao`) and editable Occlusion layer, which can feed same-set procedural Roughness graphs. No correspondence cage, curvature/thickness, dilation, project-persisted baked-map assets or broader generator inputs; editor workflow has headless but no desktop visual acceptance |
| Smart Materials / Smart Masks | ❌ | Not implemented |
| `.spp` read | ❌ | HDF5 header identification only (where applicable) |
| `.spp` write | ❌ | Not implemented |
| `.sppr` brush/tool presets | ❌ | Not implemented |
| `.spsm` / `.spmsk` | ❌ | Not implemented |
| `.spexp` | ❌ | Not implemented |
| Substance Painter Python export preset JSON | 🟡 | Export profile writes a bounded, generated `name` + `maps` JSON payload using Adobe's documented Painter Python API fields and standard PNG map channels. This is not the on-disk `.spexp` shelf format and has not been opened in Painter. PNG map import/export is supported separately. |
| `.sbsar` | ❌ | No Substance graph runtime |
| `.abr` | ❌ | Not implemented |
| GPU baking | ❌ | Not implemented |
| PNG exports incl. ORM / Unity | 🟡 | Eleven named maps, fixed pack recipes, Opacity-red-derived Base Color alpha, opt-in UV-aware padding and selectable OpenGL/DirectX Normal green-channel convention have pixel tests. Blender 5.2.2 single/multi-set smokes load exported Base Color, ORM, Normal and Emissive into Principled materials and verify links after `.blend` reopen. The multi-set CPU Cycles fixture checks ORM/normal texels and confirms Roughness, Metallic and Normal maps affect rendered response. Broad render quality and Godot/Unity parity remain unverified. |
| Kalcite frontend | ❌ | Optional future integration only; not a dependency |

## Compatibility testing policy

- Keep `.spp` at signature identification only unless lawful, documented interoperability is established and separately authorized.
- Do not infer import or round-trip support from HDF5/container recognition.
- For supported standard formats, record the exact tested features, version, preservation limits and pixel-level round-trip evidence.
- Preserve unknown fields in future native and interchange parsers where the format permits it.

### Legacy project recovery identities

Native v1–v5 archives derive stable identities from their bounded archive bytes;
normal v9 saves persist that identity. Existing v6+ IDs are preserved. Journals
created by older builds with random legacy IDs remain rejected and preserved;
they are not automatically migrated or replayed. Rejected recovery data is
retained during ordinary Save/Discard, and Save As to a different path can start
fresh recovery. This does not add Substance project compatibility.

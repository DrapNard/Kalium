# Compatibility status (2026-10-18)

**Legend:** ✅ Core behavior has automated evidence; 🟡 Partial or visually unverified; ❌ Missing. The current Linux worktree passes the local checks in `docs/qa/test-matrix.md`; GUI and downstream applications have not been visually tested.

| Feature/format | Status | Reality |
|---|---|---|
| Kalium project `.kalium` | ✅ | Editable ZIP/JSON/PNG format, versions 1–6; bounded loading, forward-compatible JSON metadata retention, validated sibling-temp recovery, manual checkpoints, and project-identity-bound recovery journals for committed image edits plus supported layer property/order/selection edits; app-level recovery tests also cover truncated final frames. Oversized patches and layer creation/removal/duplication, set, mesh, binding, undo/redo and other unsupported edits disable journal recovery until save. |
| OBJ import | 🟡 | Triangulated geometry and `.mtl` slot names with per-triangle IDs; source-byte and material-path limits; limited flattened mesh representation |
| glTF/GLB import | 🟡 | Triangle primitives, position/normal/UV0/material slots; node TRS baked into geometry; source/buffer limits; unused images skipped; tangents/additional UV sets/skin/morph data not retained |
| BaseColor / Roughness / Metallic / Normal / Height / Emissive / Opacity / AO | 🟡 | Eight editable raster channels. Base Color and Emissive brush source-over and layer compositing use linear-light RGB; Normal stamps normalize signed-vector interpolation, normal layers interpolate unit vectors, and Detail Normal uses reoriented normal mapping. Numeric headless tests cover these paths. Scalar channels blend normalized data values directly. Export alpha semantics and GUI visual behavior remain incomplete or unverified. |
| Layer opacity/visibility/masks | 🟡 | Normal, Multiply, Add and Detail Normal modes are implemented; Detail Normal applies RNM to the Normal channel and Normal semantics to other channels. Bounded undo/redo records layer add, duplicate, imported texture layers, remove, reorder and property edits; unit tests cover mixed pixel/stack/property replay, imported-layer restoration and history limits. Mask and channel editing exist in the core/UI, but interactive GUI behavior is unverified; groups and fill layers are absent. |
| 3D paint strokes | 🟡 | CPU UV hit buffer, tested stroke interpolation and 64×64 tile-sparse stroke undo; a 4K small-dab fixture verifies capture stays under 40 KiB. GUI visual behavior and seamless projection remain unverified |
| GPU rendering | 🟡 | egui UI uses wgpu; 3D mesh preview is the CPU reference rasterizer, native GPU painting is future work |
| Physically based preview | 🟡 | Deterministic CPU Cook–Torrance GGX direct-light reference with metallic/roughness, tangent normals, emissive and ambient fill; no IBL, environment controls or GUI image acceptance |
| Brush pressure/tilt/rotation | ❌ | Mouse only at the moment |
| Multiple editable Texture Sets | ✅ | Independent editable stacks, material bindings, tested headless paint/save/reopen/export workflow; GUI workflow remains unverified |
| UDIM | ❌ | Single 0–1 UV canvas per set, no UDIM tile addressing |
| Mesh-map baking | 🟡 | Headless same-mesh world-normal and normalized-position PNG bake with UV coverage metadata. Internal high-to-low world-normal projection is tested but requires already aligned coordinates; Same-mesh AO has a bounded deterministic CPU CLI bake (`bake-ao`) with PNG output; ID/curvature/thickness, dilation, persistent project maps and generator use are not implemented |
| Smart Materials / Smart Masks | ❌ | Not implemented |
| `.spp` read | ❌ | HDF5 header identification only (where applicable) |
| `.spp` write | ❌ | Not implemented |
| `.sppr` brush/tool presets | ❌ | Not implemented |
| `.spsm` / `.spmsk` | ❌ | Not implemented |
| `.spexp` | ❌ | Not implemented |
| `.sbsar` | ❌ | No Substance graph runtime |
| `.abr` | ❌ | Not implemented |
| GPU baking | ❌ | Not implemented |
| PNG exports incl. ORM / Unity | 🟡 | Eleven named maps, fixed pack recipes, Opacity-red-derived Base Color alpha, opt-in UV-aware padding and selectable OpenGL/DirectX Normal green-channel convention have pixel tests. A Blender 5.2.2 smoke acceptance loads exported Base Color, ORM, Normal and Emissive into a Principled material, verifies channel links and reopens the saved `.blend`; broad engine parity and rendered appearance remain unverified. |
| Kalcite frontend | ❌ | Optional future integration only; not a dependency |

## Compatibility testing policy

- Keep `.spp` at signature identification only unless lawful, documented interoperability is established and separately authorized.
- Do not infer import or round-trip support from HDF5/container recognition.
- For supported standard formats, record the exact tested features, version, preservation limits and pixel-level round-trip evidence.
- Preserve unknown fields in future native and interchange parsers where the format permits it.

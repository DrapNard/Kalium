# Compatibility status (2026-10-13)

**Legend:** ✅ Working in prototype source (not independently compiled here); 🟡 Partial; ❌ Missing.

| Feature/format | Status | Reality |
|---|---|---|
| Kalium project `.kalium` | ✅ | ZIP/JSON/PNG fully editable, versioned |
| OBJ import | ✅ | Triangulated OBJ with source `.mtl` slot names and per-triangle material IDs; mesh rescaled for viewport |
| glTF/GLB import | ✅ | Triangle primitives, 1st UV set and material slot IDs/names; embedded PBR textures not loaded |
| BaseColor / Roughness / Metallic / Normal / Height / Emissive / Opacity / AO | ✅ | Per-layer 8-channel raster painting |
| Layer opacity/visibility/masks | ✅ | Normal, Multiply and Add blending |
| 3D paint strokes | 🟡 | CPU UV hit buffer; projection doesn't paint through UV seams automatically |
| GPU rendering | 🟡 | wgpu UI compositing; CPU 3D rasterizer |
| Physically accurate PBR | 🟡 | Simple preview, not a reference metallic/roughness BRDF |
| Brush pressure/tilt/rotation | ❌ | Mouse only at the moment |
| Multiple editable Texture Sets | 🟡 | Independent editable stacks, bindings by mesh material, CPU multi-material preview and per-set exports |
| UDIM | ❌ | Single 0–1 UV canvas per set, no UDIM tile addressing |
| Material ID masks, baking AO/curvature/normal | ❌ | Not implemented |
| Smart Materials / Smart Masks | ❌ | Not implemented |
| `.spp` read | ❌ | HDF5 header identification only (where applicable) |
| `.spp` write | ❌ | Not implemented |
| `.sppr` brush/tool presets | ❌ | Not implemented |
| `.spsm` / `.spmsk` | ❌ | Not implemented |
| `.spexp` | ❌ | Not implemented |
| `.sbsar` | ❌ | No Substance graph runtime |
| `.abr` | ❌ | Not implemented |
| GPU baking | ❌ | Not implemented |
| PNG exports incl. ORM / Unity | ✅ | 11 named maps, with fixed pack recipes |
| Kalcite frontend | ❌ | Optional future integration only; not a dependency |

## Compatibility testing policy

- Build a legally obtained corpus of projects created with different versions of Substance Painter.
- Record each tested version, data structures, number of layers, masks and Texture Sets.
- Write clean-room parsers in isolated `formats/` crates that preserve unknown blocks and assert byte-level round-tripping where appropriate.
- Never claim full round-trip compatibility until Painter successfully opens the rewritten file and editable elements are proven retained.
- Maintain separate "container recognized", "metadata parsed", "preview restored", "editable import" and "safe editable export" flags.

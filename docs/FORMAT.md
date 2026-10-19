# Kalium native format — manifests v1 through v8

The `.kalium` extension describes an independent ZIP/JSON/PNG container. It contains no Adobe binary code or resources. Version 3 introduces separate editable Texture Sets; version 4 adds per-triangle mesh materials and bindings; version 5 adds the `Detail` blend mode for reoriented Normal composition; version 6 adds a stable project UUID; version 7 adds bounded procedural roughness graphs on layers; version 8 adds same-set AO layer graph inputs. Versions 1–7 continue to load; saves write v8.

## v3 example

```json
{
  "format": "Kalium Project",
  "version": 3,
  "mesh": {"name": "Cube", "vertices": [], "indices": []},
  "size": 512,
  "selected": 0,
  "next_layer_id": 2,
  "layers": [
    {"id":1,"name":"Paint Layer","visible":true,"opacity":1.0,
     "blend":"Normal","channels":["BaseColor"],"disabled_channels":[]}
  ],
  "active_set_id": 1,
  "active_set_name": "Default",
  "next_set_id": 3,
  "texture_sets": [
    {"id":2,"name":"Armor","size":1024,"selected":0,"next_layer_id":2,
     "layers":[{"id":1,"name":"Paint Layer","visible":true,"opacity":1.0,
                "blend":"Normal","channels":["Roughness"],"disabled_channels":[]}]}
  ]
}
```

The example mesh array is abbreviated; actual projects contain a nonempty, validated triangle mesh. Every set may use a different power-of-two or valid custom size, up to 4096. `next_set_id` is strictly larger than all existing IDs; IDs are stable across edits. `texture_sets` contains **only inactive** sets; the active set remains represented by the legacy top-level `size`, `selected`, `next_layer_id` and `layers` properties. This choice enables old painting APIs to edit the active set without extra memory copies.

## ZIP paths

```text
manifest.json
layers/{layer_id}/mask.png
layers/{layer_id}/BaseColor.png
layers/{layer_id}/{channel_name}.png
texture_sets/{set_id}/layers/{layer_id}/mask.png
texture_sets/{set_id}/layers/{layer_id}/{channel_name}.png
```

The root `layers/` refers to **the active set**, whatever its `active_set_id`. Each `texture_sets/{set_id}/...` directory contains an inactive set. Every layer has an 8-bit grayscale mask; a listed channel has an RGBA PNG. An unlisted channel is treated as transparent. `disabled_channels` records non-destructively disabled channels. The mesh has top-left UV origin.

## Compatibility and restrictions

- **v1:** no per-layer `disabled_channels` and no Texture Set metadata; all channels enabled by default.
- **v2:** adds `disabled_channels`.
- **v3:** adds active and inactive Texture Set state and unique paths for set resources. Older Kalium versions cannot read it.
- **v4:** records `mesh.materials`, `mesh.triangle_materials` and `material_bindings` keyed by stable Texture Set IDs; the legacy forms remain importable.
- The parser rejects extra unknown ZIP files, duplicate archive names, duplicate/invalid set IDs, invalid image sizes, invalid source mesh data, and a retained texture plus image-loading staging peak above 512 MiB (including one encoded image entry and decoder scratch). Up to 16 sets and 64 layers per set are supported.
- ZIP member names use numeric IDs, not user-controlled set names. PNG exports use a sanitized name with the stable set ID to avoid traversal and collisions.
- Unknown JSON fields at supported manifest levels are retained through load/save. Unknown ZIP payload files are rejected to avoid lossy saves. Preserve backups before migrating valuable projects to a newer manifest version.
- Mesh materials can be assigned to different sets. The viewport still rasterizes on the CPU. There is no implied compatibility with `.spp`.


## Native project v4 — material slot bindings (2026-10-13)

The v4 JSON manifest adds `material_bindings: [null | texture_set_id, ...]` with one entry per imported mesh material slot. `null` falls back to the active Texture Set; a non-null value is a stable Texture Set identifier. The manifest's `mesh` adds optional `materials: [string, ...]` and `triangle_materials: [u32, ...]` (one per triangle, indexed by materials). Their absence on legacy meshes means material slot 0. The importer accepts v1–v3 manifests and defaults their binding table to all `null`.

Mesh vertices may include an optional `tangent: [x, y, z, handedness]` field. It stores glTF tangent direction and bitangent handedness after node-transform baking; the CPU reference renderer uses it for tangent-space normal maps. Older vertex records omit the field and load with no stored tangent, so this addition does not require a project manifest version change. Project validation rejects nonfinite, nonunit, nonorthogonal tangent frames.

v4 validates the binding vector length, the slot indices, and references to existing Texture Sets. Existing layer PNG paths are unchanged. No SPP-specific format is written.

## Native project v5 — Detail Normal layer blending (2026-10-18)

Version 5 adds `Detail` to the layer `blend` enum. For the Normal channel, the compositor weights the top tangent-space detail normal against neutral `(0, 0, 1)` using pixel alpha × layer opacity × mask, then applies reoriented normal mapping (RNM) to the accumulated normal. For other channels, `Detail` behaves like `Normal`. Older versions cannot decode projects containing the new enum value; v5 introduced the `Detail` encoding and remains readable in v6–v8. The v1–v4 readers remain supported.


## Native project v6 — Stable project identity (2026-10-18)

Version 6 writes a non-nil UUID in the root `project_id` manifest field. The UUID remains stable across saves and is reserved for associating recovery journals with the correct project and saved baseline; the current framing layer checks this UUID and a bounded SHA-256 of the saved archive. Save As to a distinct project path creates a new identity; a regular save preserves it. When a v1–v5 project is loaded, Kalium assigns an in-memory UUID; the identity becomes persistent when the migrated project is saved as v6 or later. Older versions do not contain this identity field.

## Native project v7 — Procedural roughness graphs (2026-10-18)

Version 7 adds an optional `procedural_graph` object to layer metadata. Its original bounded graph is UV input → seeded scalar value noise → Roughness output. Graphs are validated on load and evaluated deterministically by the CPU compositor; graph output is not embedded as a PNG layer channel. The Layers pane exposes seed and scale controls. Versions 1–6 remain readable and are migrated on save. Later versions retain v7 graph support.

The v7 implementation also stores optional AO bake options (`sample_count`, `max_ray_distance`, and `seed`) in the reserved `kalium_bake_ao` Texture Set extension. For the active set, this extension is flattened at the manifest root under the existing compatibility layout; inactive sets store it in their `texture_sets[]` object. The extension does not change the manifest version. Older projects use default AO options, and unknown keys nested in this extension remain preserved when the options are edited.

## Native project v8 — AO-driven Roughness graphs (2026-10-18)

Version 8 extends the bounded graph with an optional `OcclusionInput` node and a scalar `Multiply` node. The input references a stable layer ID within the same Texture Set and reads that layer's stored Occlusion red channel by matching texel coordinates. The graph multiplies seeded value noise by AO to produce Roughness. Source visibility, opacity, mask and channel-enable state do not change the sampled data. When the destination layer also has authored Roughness pixels, their alpha overlays the generated result; the destination layer's opacity, mask and blend mode then apply once. If the source layer or its Occlusion map is missing, the input is neutral white and the graph falls back to seeded noise, without retargeting another layer. The referenced AO remains ordinary editable layer content and follows project save, Undo/Redo and recovery behavior. Versions 1–7 remain readable and migrate on save; older Kalium builds reject v8 projects.

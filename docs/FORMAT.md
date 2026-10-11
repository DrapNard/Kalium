# Kalium native format — manifests v1, v2 and v3

The `.kalium` extension describes an independent ZIP/JSON/PNG container. It contains no Adobe binary code or resources. Version 3 introduces separate editable Texture Sets. Version 1/2 projects continue to load and are migrated in memory to one set named `Default`.

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
- The parser rejects extra unknown ZIP files, duplicate archive names, duplicate/invalid set IDs, invalid image sizes, invalid source mesh data, and a summed decoded texture budget above 512 MiB. Up to 16 sets and 64 layers per set are supported.
- ZIP member names use numeric IDs, not user-controlled set names. PNG exports use a sanitized name with the stable set ID to avoid traversal and collisions.
- Unknown format fields cannot yet round-trip; preserve backups of valuable projects before migrating to v3.
- These sets are **not yet assigned to mesh materials**. Kalium's current CPU viewport displays one active material over all triangles. There is no implied compatibility with `.spp`.

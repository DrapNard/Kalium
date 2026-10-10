# Kalium v1 file format

Extension `.kalium`. ZIP archive (deflate compression) with UTF-8 JSON `manifest.json` and PNG layer images. The format is independently documented and does not contain Adobe files.

## Manifest

```json
{
  "format": "Kalium Project",
  "version": 1,
  "size": 512,
  "mesh": {"name": "Cube", "vertices": [], "indices": []},
  "selected": 0,
  "next_layer_id": 2,
  "layers": [
    {
      "id": 1,
      "name": "Paint Layer",
      "visible": true,
      "opacity": 1.0,
      "blend": "Normal",
      "channels": ["BaseColor", "Roughness"]
    }
  ]
}
```

`mesh.vertices` is an array of `{ "position":[f32,f32,f32], "normal":[...], "uv":[f32,f32] }`. `mesh.indices` is a flattened index list of triangles. UV origin is texture top-left.

Each layer must have `layers/{id}/mask.png` (8-bit grayscale). Each `channels[]` entry must correspond to `layers/{id}/{ChannelFilename}.png` (8-bit RGBA). Unlisted channel files are absent, indicating a fully transparent paint layer for that channel. All images have dimensions `size × size`. Layer textures contain stroke alpha; the displayed result is computed over channel defaults with the layer's mask, blend and opacity.

Default base channel values are documented in `src/document.rs`. The format currently stores a normalized mesh. Mesh import/export history and source transforms are not preserved separately.

## Robustness

The reader validates archive entries, dimensions, layers, triangle limits, floats, known version number and decompressed size per member. JSON schema migrations and unknown-field preservation are future work. As with any alpha file format, keep a backup before opening valuable art.

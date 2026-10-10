# Kalium brush preset format (`.kbrush`, version 1)

The editable, readable `.kbrush` format is **Kalium-native**, not Adobe Substance Painter's `.sppr` format. No commercial source assets are included.

```json
{
  "format": "Kalium Brush",
  "version": 1,
  "name": "Soft Painter",
  "brush": {
    "radius": 32,
    "opacity": 0.65,
    "hardness": 0.2,
    "spacing": 0.15,
    "mode": "Paint",
    "color": [225, 99, 137, 255],
    "grayscale": 175,
    "mirror_u": false
  }
}
```

Valid modes: `Paint`, `Erase`, `MaskWhite`, `MaskBlack`, `Sample`. Radius: 1–4096 px. Opacity and hardness: 0–1. Spacing: 0.1–4.0. The file input limit is 64 KiB; unknown fields and unsupported versions are rejected instead of silently discarded.

Kalium can load and save these via File → Load/Save brush preset, or `kalium brush-inspect path.kbrush`. The legacy `.sppr` interoperability research remains a future target.

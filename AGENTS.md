# Development agent guide

Read `README.md`, `PHILOSOPHY.md`, `CONTRIBUTING.md`, then `docs/COMPATIBILITY.md` and `docs/ROADMAP.md`.

Rules:
- Rust 2021, AGPL-3.0-only. Community must not need Kalcite to build.
- Never claim an implemented feature without tests or inspection of actual running behavior.
- No generative AI image/material features for the official product. AI-assisted code development is permitted with human review.
- No proprietary Adobe source, shaders, resources or undocumented copied assets in the codebase.
- Add tests for any bugfix, especially project corruption, image blending and mesh picking.
- Treat imported files as untrusted, cap allocations and recursion, preserve unknown fields in future parsers.
- The CPU 3D rasterizer is a **reference implementation**. Native GPU compute painting is future work.
- The `.spp` detector is NOT an importer. The target is lawful clean-room format interoperability.


## Editor workspace (`v0.2.0-alpha.3`)

- `src/layout.rs` owns the serializable docking tree and platform configuration location.
- `src/app.rs` implements `egui_dock::TabViewer`. Keep every panel functional and independent; a hidden panel must never hide/delete project content.
- UI layout is a per-user preference, **never** stored inside `.kalium` files. Use the View menu to restore closed panes.
- Match the provided reference screenshot in *structure and interactions* without bundling/copying its art, proprietary icons, branding or generated images.
- **No image generation** is to be used in Kalium development unless the user explicitly changes that instruction.
- Preserve the existing CPU painting reference and experimental GPU feature flag; never claim `.spp` project import works unless real round-trip tests pass.

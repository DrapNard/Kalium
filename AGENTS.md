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

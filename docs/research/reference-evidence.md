# Reference evidence log

Updated 2026-10-18. No REA/Ghidra analysis was needed for the current workflow-safety slice. No proprietary source, assets, shaders, or reverse-engineered material was used.

| Date | Claim | Class | Evidence | Use/limit |
|---|---|---|---|---|
| 2026-10-18 | Kalium stores artist layers, masks and mesh data in its own versioned `.kalium` ZIP format. | OBSERVED | Current `src/project.rs`; project roundtrip tests pass in `cargo test --locked` | This does not establish compatibility with `.spp`. |
| 2026-10-18 | Current 3D display and hit mapping are produced by a CPU software rasterizer. | OBSERVED | `src/renderer.rs` module docs and implementation | wgpu is used by eframe presentation, not a native GPU mesh painter. |
| 2026-10-18 | Unsaved New/Open could replace the current document without confirmation. | OBSERVED | `src/app.rs` `perform(Action::New/Open)` before UI-001 | Fix is in progress and must be rechecked after integration. |
| 2026-10-18 | Painter's public docs are the preferred source for user-visible compatibility behavior. | DOCUMENTED | [Substance 3D Painter manual](https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/home) | Consult only for a concrete clean-room behavior question; no such question was needed for this slice. |
| 2026-10-18 | glTF material and mesh semantics should be checked against the public format specification. | DOCUMENTED | [Khronos glTF 2.0 specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html) | No new glTF behavior is claimed in this checkpoint. |

REA evidence: none consulted this session. Existing inventory identifiers in the roadmap are historical context only, not evidence of product behavior.

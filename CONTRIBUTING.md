# Contributing to Kalium

Welcome! The project is Rust-first: knowing Kalcite is **not** required. No new custom programming language is required to compile or change the official app.

## Setup

1. Install the current stable Rust toolchain, a Vulkan-compatible graphics stack on Linux, and the native libraries required by eframe.
2. Run `cargo test`, `cargo fmt --all`, and `cargo clippy --all-targets`.
3. For a feature, include a small example asset that is self-created or appropriately freely licensed and a regression test.
4. Give precise user-visible compatibility status. Never claim `.spp` round-trip, SBSAR execution, or Painter brush parity until test-corpus evidence exists.
5. Never import or distribute Adobe source, private SDK binaries, proprietary sample resources, or reverse-engineered internal code. Use public specifications and lawful observation for clean-room interoperability.
6. Keep file decoders hardened against malformed data, huge allocation requests, invalid floats and unknown schema versions.
7. Avoid new dependencies unless they materially simplify the problem. Explain their licenses.

## Structure

- `src/mesh.rs` — OBJ/glTF/GLB import and normalized triangle mesh.
- `src/document.rs` — multichannel non-destructive layers and blending.
- `src/paint.rs` — brush engine.
- `src/renderer.rs` — CPU reference renderer and GPU-uploadable preview; eventual wgpu native replacement.
- `src/history.rs` — paint stroke undo/redo.
- `src/project.rs` — editable native .kalium archives.
- `src/export.rs` — PBR texture export packs.
- `src/spp.rs` — **SPP identification only**, no structural parser.
- `src/app.rs` — egui desktop shell. Do not add rendering/format parsing logic here.

## Standards

- Preserve artists' work: unknown data should never be silently dropped.
- Return `Result`, avoid panics on untrusted files.
- Separate importer/external format data from native document model.
- Keep painting deterministic; tests should compare pixels and round trips.
- Follow the [Artist-First Charter](PHILOSOPHY.md).

Code is AGPL-3.0-only. By contributing you agree your contributions may be distributed under this license, subject to attribution obligations for third-party work.

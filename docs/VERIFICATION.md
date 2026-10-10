# Build and verification status — v0.2.0-alpha.5

Maintainer / version identity: **DrapNard**.

This source iteration was prepared without installed `rustc`/`cargo` and without internet access to install them. In this packaging environment no Rust toolchain is installed. The user reported that 28 of the 29 tests in v0.2.0-alpha.4 passed on Arch Linux; the single failed test was `layout::tests::workspace_json_roundtrips` and its panic included `invalid type: null, expected f32`. These are *user-provided previous-version* results, not verification that this new iteration compiles. The new Rust and WGSL code have not been compiled here. The presence of a CI job is not evidence that CI passed.

## Checks performed for the downloadable package

- [x] `Cargo.toml` parses as TOML and declares version `0.2.0-alpha.4`, AGPL-3.0-only, DrapNard and an opt-in compute feature.
- [x] Example `.kalium` ZIP CRC check and manifest header inspection.
- [x] Example `.kbrush` JSON format/version parsing.
- [x] Source ZIP integrity check (`ZipFile.testzip`) and extracted source layout.
- [x] Git bundle verified with `git bundle verify`, version tags and isolated clone check.
- [x] `git diff --check` before commit.
- [x] Static delimiter inspection on the edited Rust sources. This is **not** a Rust parse or compilation test.
- [x] Workspace source contains independent 3D/UV editors, `egui_dock` dependency pinned to the egui 0.31-compatible version, eight closable panels and JSON workspace persistence.
- [x] Source assertions for reported float literal widths and the must-use Texture Set row warning.
- [x] The previous layer duplication, mask editing and framebuffer size tests exist; new tests cover workspace serialization and PBR channel switches.
- [ ] **Rust compilation / unit tests:** not run; compiler not present.
- [ ] GUI opened to verify panel placement / dragging. Not available here.

## Checks still required on a host with Rust and a GPU

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo run --release # drag, undock, close/reopen panels and restart to test persistence
cargo check --all-targets --features gpu-experimental
cargo test --all-targets --features gpu-experimental
cargo run -- validate examples/Kalium-Demo.kalium
cargo run -- uv-mask examples/Kalium-Demo.kalium ./coverage.png
cargo run --features gpu-experimental -- gpu-stamp input.png output.png 0.5 0.5 24
```

After these, manual testing is needed in the native GUI for painting, imported UV layouts, flood-fill, masks, undo and redo. Compare GPU compute brush results with the CPU brush at multiple opacities and hard/soft edges. Compatibility with `.spp` is **not** implemented.

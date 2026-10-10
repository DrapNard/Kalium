# Build and verification status — v0.2.0-alpha.2

Maintainer / version identity: **DrapNard**.

This source iteration was prepared without installed `rustc`/`cargo` and without internet access to install them. None of the Rust code or WGSL shader has been compiled, run, or hardware-validated here. The presence of a CI job is not evidence that CI passed.

## Checks performed for the downloadable package

- [x] `Cargo.toml` parses as TOML and declares version `0.2.0-alpha.2`, AGPL-3.0-only, DrapNard and an opt-in compute feature.
- [x] Example `.kalium` ZIP CRC check and manifest header inspection.
- [x] Example `.kbrush` JSON format/version parsing.
- [x] Source ZIP integrity check (`ZipFile.testzip`) and extracted source layout.
- [x] Git bundle verified with `git bundle verify`, version tags and isolated clone check.
- [x] `git diff --check` before commit.

## Checks still required on a host with Rust and a GPU

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo check --all-targets --features gpu-experimental
cargo test --all-targets --features gpu-experimental
cargo run -- validate examples/Kalium-Demo.kalium
cargo run -- uv-mask examples/Kalium-Demo.kalium ./coverage.png
cargo run --features gpu-experimental -- gpu-stamp input.png output.png 0.5 0.5 24
```

After these, manual testing is needed in the native GUI for painting, imported UV layouts, flood-fill, masks, undo and redo. Compare GPU compute brush results with the CPU brush at multiple opacities and hard/soft edges. Compatibility with `.spp` is **not** implemented.

# Kalium v0.3.0-alpha.2 — verification report (source package)

Author / release identity: **DrapNard**. The previously reported **35/35 passing tests belong to v0.2.0-alpha.5.2**, not to this release.

## Verified in the packaging environment

- `Cargo.toml` parsed as TOML, release version and license checked.
- All Rust source files passed a **static balanced-delimiter scan** (this is not a Rust compiler/parser).
- `git diff --check` passed before commit.
- Legacy `.kalium` v1 demo ZIP and manifest readable by an independent ZIP/JSON parser.
- New `examples/TwoMaterials.obj` / `.mtl` fixture supplied, with separate `Leather` and `Metal` material groups.
- Release ZIP and Git bundle integrity verified after packaging.

## NOT validated here

- **No Rust toolchain** installed; `cargo check`, `cargo test`, `cargo fmt` and compilation of the opt-in GPU experiment were not run.
- No interactive rendering/painting test on a workstation.
- No Substance `.spp` or SBSAR compatibility is claimed.

## Arch Linux maintainer commands

```bash
cargo fmt --all
cargo check --all-targets
cargo test --all-targets
cargo check --all-targets --features gpu-experimental
cargo run --release
```

Manual smoke: `File → Import mesh` and choose `examples/TwoMaterials.obj`, add a second Texture Set, bind the `Metal` mesh material to it in **Texture Set List**, paint different colors on each side, save `.kalium`, reopen and confirm bindings. Keep the `.mtl` file in the same folder as the OBJ. Verify the left and right materials remain distinct when switching active sets.

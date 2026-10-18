# Kalium v0.3.0-alpha.2 — verification report

Author / source identity: **DrapNard**. This report separates the current source-worktree checks from the earlier packaging checks; neither establishes a production release.

## Current worktree checks (2026-10-18)

The 192-test commit `1316e71` was verified from an isolated clean Linux clone; the latest worktree adds app-level checksum-corruption rejection and has 193 tests:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test --locked --all-targets` — 193 passed, 0 failed, including layer-row drag/reorder, selected-layer restoration, oversized-stroke fail-closed recovery, torn-tail prefix restore/append, corrupt-frame rejection without a recovery prompt, and save → journal append → reopen → image/layer replay and Discard coverage.
- `cargo clippy --locked --all-targets -- -D warnings`.
- `cargo check --locked --offline --all-targets`.
- `cargo check --locked --all-targets --features gpu-experimental` — compile only.
- CLI validation/preview/export and mesh-map smoke checks, plus `bake-ao` on the demo project (512×512 RGBA PNG; 262,144 covered texels).
- Blender 5.2.2 import, PBR map-link, save and reopen smoke using `scripts/qa/blender_import_smoke.py`.

These checks describe source code, not a packaged release. Commit `1316e71` passed the complete gate sequence from a fresh isolated clone: formatting, 192 tests, strict Clippy, GPU-experimental compilation, diff check and clean status. The previous commit `9fc6f91` passed the same checks at 191 tests. Commit `12bc283` tracks `Cargo.lock`; commit `28a5555` includes the source cleanup for the rustfmt and strict-Clippy gates. An isolated clone at `7883e51` passed `cargo fmt --all -- --check`, `cargo test --locked --all-targets` (186/186), strict Clippy, GPU-experimental feature compilation, and `git diff --check`. The configured origin is a local bundle and hosted CI has not run.

## Earlier packaging checks

- `Cargo.toml` parsed as TOML, and release version and license were checked.
- Rust source files passed a static balanced-delimiter scan (not a Rust compiler/parser).
- Legacy `.kalium` v1 demo ZIP and manifest were readable by an independent ZIP/JSON parser.
- `examples/TwoMaterials.obj` / `.mtl` provides separate `Leather` and `Metal` material groups.
- Release ZIP and Git bundle integrity were checked after packaging.

## Not validated

- No interactive GUI painting, undo/redo, rendering or full user workflow was validated; this environment has no working display/compositor or Vulkan driver.
- GPU hardware behavior, cross-platform builds, packaged artifacts and remote CI remain unverified.
- No Substance `.spp` or SBSAR import/export compatibility is claimed.
- No latency benchmark or whole-application memory profile exists. Interactive stroke undo captures touched 64×64 tiles; fill/clear history and material-composite invalidation remain full-image costs.

## Maintainer commands

```bash
cargo fmt --all
cargo check --locked --all-targets
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --all-targets --features gpu-experimental
cargo run --release
```

For GUI acceptance, import `examples/TwoMaterials.obj`, paint separate material regions, save/reopen `.kalium`, and export maps. Keep the `.mtl` beside the OBJ and verify bindings and per-set results. The automated headless flow and its limits are listed in `docs/qa/test-matrix.md`.

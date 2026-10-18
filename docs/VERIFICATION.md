# Kalium v0.3.0-alpha.2 — verification report

Author / source identity: **DrapNard**. This report separates the current source-worktree checks from the earlier packaging checks; neither establishes a production release.

## Current worktree checks (2026-10-18)

The dirty Linux worktree passed:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test --locked --all-targets` — 186 passed, 0 failed, including a headless egui layer-row drag/reorder interaction and save → journal append → reopen → image, layer-property and layer-order replay and Discard coverage.
- `cargo clippy --locked --all-targets -- -D warnings`.
- `cargo check --locked --offline --all-targets`.
- `cargo check --locked --all-targets --features gpu-experimental` — compile only.
- CLI validation/preview/export smoke checks and the same-mesh mesh-map bake smoke described in `docs/roadmap/session-handoff.md`.
- Blender 5.2.2 import, PBR map-link, save and reopen smoke using `scripts/qa/blender_import_smoke.py`.

These checks describe the current source tree, not a packaged release. Commit `12bc283` tracks `Cargo.lock`; commit `28a5555` includes the source cleanup for the rustfmt and strict-Clippy gates. The working checkout passes all listed commands. The full command set from an isolated clone of `28a5555` remains to be run. The configured origin is a local bundle and hosted CI has not run.

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

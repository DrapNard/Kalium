# Kalium v0.3.0-alpha.2 — verification report

Author / source identity: **DrapNard**. This report separates the current source-worktree checks from the earlier packaging checks; neither establishes a production release.

## Current worktree checks (2026-10-19)

The last clean committed checkpoint, `9759b4f`, passed from an isolated Linux clone with 194 tests. The current shared worktree includes uncommitted fixes to project archive loading, texture import budgeting and recovery, glTF scene selection, bounded journal events, synthetic paint interaction coverage, Texture Set List interaction coverage, glTF tangent preservation/shading and source-package CI; it passes:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test --locked --offline --all-targets` — 479 passed, 0 failed, 2 ignored (fixture generator and manual performance benchmark). This run includes classic ZIP/ZIP64 directory preflight (entry count, aggregate metadata and record bounds) before archive construction; glTF TEXCOORD_1–7 preservation, UV0 requirement, UV8+ rejection, and fail-closed deformation handling; bounded Activity persistence and pane rendering; layer-row selection dirty-state, save/reopen and journal Restore; S4 tree primitive validation (not yet in the document path); near-equal duplicated-vertex UV seam painting with atlas-gap isolation, workspace close-time flush and atomic layout replacement, resize peak-memory bounds, UV zoom/pan painting with save/reopen parity, and OBJ short-face preflight expansion checks. It also includes the editor material-ID export workflow: synthetic click routing, shared CLI/UI metadata, exact decoded PNG match, cancellation without artifacts, output-pair rollback and document immutability. It also includes hard glTF reader byte and full-document node caps, repeated/unreachable node fixtures, tangent transform/persistence/renderer response tests with missing-data fallback, deterministic Studio/Warm/Cool renderer tests, roughness-dependent environment response, environment/cache full-render parity, a synthetic environment selector interaction, and focused-text-editor Undo/Redo routing.
- `cargo clippy --locked --all-targets -- -D warnings`.
- Async recovery writer tests cover FIFO acknowledgments, item/byte backpressure, write-failure propagation, and draining accepted work. A paused-worker app test verifies Undo does not mutate pixels or history before its recovery record is durably acknowledged.
- `cargo check --locked --all-targets --features gpu-experimental` — compile only.
- `cargo package --locked --offline --allow-dirty` — passed locally; Cargo packaged 64 files and successfully verified the staged package build. The hosted CI workflow now runs `cargo package --locked` on its clean checkout.
- `scripts/qa/cli_release_smoke.sh target/debug/kalium` — PASS; validates the demo project, preview/UV/AO/mesh-map outputs, all 11 maps for generic, Godot, Unity and glTF profiles, profile manifests, PNG signatures/dimensions, mesh-map metadata and unchanged source-project SHA-256. CI builds the binary and runs this artifact check.
- `bake-high-to-low` CLI tests — PASS; an aligned OBJ fixture produces a world-normal PNG matching the direct bake API, a coverage PNG and assumption metadata; invalid ray distances and budget underflow reject, and the source `.kalium` file remains byte-identical.
- CLI validation/preview/export and mesh-map smoke checks, plus `bake-ao` and `bake-material-id` on the demo project. Material-ID writes a 512×512 PNG with a `.png.json` sidecar describing exact 24-bit little-endian RGB slot encoding and alpha coverage; CLI and editor share metadata and staged output-pair publication. Tests compare pixels, material slots and source-project bytes; the editor tests additionally verify cancellation and document immutability. The CLI release smoke verifies output dimensions/metadata and source immutability.
- Project save tests inject create, archive write/finish, temp-sync, replacement and parent-directory-sync failures. They verify byte-identical primary preservation before replacement and distinguish a post-replacement sync error after reopening the new archive. Unix directory sync is exercised locally; Windows replacement is source-reviewed only.
- Frame-stepped export-job parity/cancellation checks and a headless app progress/cancel state test.
- Blender 5.2.2 single-set and multi-set import/node-link/save/reopen smokes using `scripts/qa/blender_import_smoke.py` and `scripts/qa/blender_multiset_smoke.sh target/debug/kalium`. The two-set fixture checks distinct Leather/Metal paths and ORM/normal values, then CPU Cycles counterfactuals confirm Roughness, Metallic and Normal maps affect the reopened Principled render. This is not broad engine or visual-quality acceptance.
- `cargo test --locked --offline mesh::tests::` — 50 mesh tests pass, including bounded deterministic truncation and byte mutation across OBJ/MTL and glTF/GLB seeds, tangent transform/migration/project validation, and referenced-MTL error propagation. This is a regression mutation harness, not coverage-guided fuzzing or peak-memory profiling.

These checks describe source code, not a desktop installer or release artifact. The latest code changes remain uncommitted because this environment mounts `.git` read-only. Commit `9759b4f` passed the complete gate sequence from an isolated clean clone: formatting, 194 tests, strict Clippy, GPU-experimental compilation, diff check and clean status. Commit `88c5ea1` passed the same checks at 193 tests; `9fc6f91` passed at 191. Commit `12bc283` tracks `Cargo.lock`; commit `28a5555` includes the source cleanup for the rustfmt and strict-Clippy gates. An isolated clone at `7883e51` passed `cargo fmt --all -- --check`, `cargo test --locked --all-targets` (186/186), strict Clippy, GPU-experimental feature compilation, and `git diff --check`. The configured origin is a local bundle; hosted CI has not run.

## Earlier packaging checks

- `Cargo.toml` parsed as TOML, and release version and license were checked.
- Rust source files passed a static balanced-delimiter scan (not a Rust compiler/parser).
- Legacy `.kalium` v1 demo ZIP and manifest were readable by an independent ZIP/JSON parser.
- `examples/TwoMaterials.obj` / `.mtl` provides separate `Leather` and `Metal` material groups.
- Release ZIP and Git bundle integrity were checked after packaging.

## Not validated

- No interactive GUI painting, undo/redo, rendering or full user workflow was validated. `DISPLAY=:0` rejects `xdpyinfo`, `WAYLAND_DISPLAY=wayland-1` is unusable, `/dev/dri` is absent, and `vulkaninfo --summary` finds no Vulkan drivers.
- GPU hardware behavior, cross-platform builds, packaged artifacts and remote CI remain unverified.
- No Substance `.spp` or SBSAR import/export compatibility is claimed.
- The manual CPU benchmark covers tile compositing and viewport refresh/render time; no whole-application memory profile or desktop upload measurement exists. Interactive stroke undo captures touched 64×64 tiles. Uniform Fill/Clear/Reset history uses buffer swaps to avoid duplicate full-image snapshots; each edit and current material-composite invalidation still touches a full map.

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

# Publishing Kalium releases — maintained by DrapNard

Kalium is an independent project; do not publish it under an Adobe name or imply official SPP compatibility while that format is unsupported.

1. Confirm `cargo test --locked --all-targets`, `cargo check --locked --all-targets`, `cargo fmt --all -- --check` and `cargo clippy --locked --all-targets -- -D warnings` on a host with a current Rust compiler.
2. Run `cargo run --locked -- validate examples/Kalium-Demo.kalium`, `cargo run --locked -- export examples/Kalium-Demo.kalium ./exports-smoke`, and `cargo run --locked -- preview examples/Kalium-Demo.kalium ./preview.png`.
3. Open the GUI, test imported PNG, OBJ and demo `.kalium`; paint, flood-fill, UV clipping, undo/redo, save/reopen, and recover an interrupted image-edit session. For optional GPU code, also run `cargo check --locked --all-targets --features gpu-experimental` and the shader comparison plan in `docs/GPU_COMPUTE.md`.
4. Update `Cargo.toml`, `README.md`, `CHANGELOG.md`, `AUTHORS.md` and `docs/ROADMAP.md` to match the release.
5. Commit and sign/tag with the maintainer's own locally configured Git identity. Do not fabricate email addresses or signatures.
6. Publish an archive plus its SHA-256; mark source-only releases honestly and avoid claiming compiled binaries unless built and tested.

**Current source version:** `v0.3.0-alpha.2`, by DrapNard. On 2026-10-18, this Linux worktree passes 193 tests, formatting, strict Clippy, all-target compilation and optional GPU-feature compilation. Headless tests cover layer-row drag reorder and journal Restore/Discard policies; desktop GUI behavior, GPU hardware, downstream-engine imports, cross-platform builds and packaged artifacts remain unverified and must be checked before release. Journal recovery is connected for committed image edits and supported layer properties/order/selection; torn final frames restore the durable prefix and complete checksum-corrupt frames are rejected without a prompt. Remaining document mutation coverage and desktop recovery-prompt acceptance remain open.

For a portable source release, create a **Git bundle** alongside the source ZIP and verify it in a clean directory. A bundle should contain the intended history and tags; do not present reconstructed local history as pre-existing GitHub commits. Release commits must use the maintainer's own locally configured identity. The current worktree has no release ZIP or bundle artifact.

```bash
git clone Kalium-v0.3.0-alpha.2-git.bundle Kalium
cd Kalium
git tag --list
```

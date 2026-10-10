# Publishing Kalium releases — maintained by DrapNard

Kalium is an independent project; do not publish it under an Adobe name or imply official SPP compatibility while that format is unsupported.

1. Confirm `cargo test --all-targets`, `cargo check --all-targets`, `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` on a host with a current Rust compiler.
2. Run `cargo run -- validate examples/Kalium-Demo.kalium`, `cargo run -- export examples/Kalium-Demo.kalium ./exports-smoke`, and `cargo run -- preview examples/Kalium-Demo.kalium ./preview.png`.
3. Open the GUI, test imported PNG, OBJ and demo `.kalium`; paint, flood-fill, UV clipping, undo/redo, save/reopen. For optional GPU code, also run `cargo check --all-targets --features gpu-experimental` and the shader comparison plan in `docs/GPU_COMPUTE.md`.
4. Update `Cargo.toml`, `README.md`, `CHANGELOG.md`, `AUTHORS.md` and `docs/ROADMAP.md` to match the release.
5. Commit and sign/tag with the maintainer's own locally configured Git identity. Do not fabricate email addresses or signatures.
6. Publish an archive plus its SHA-256; mark source-only releases honestly and avoid claiming compiled binaries unless built and tested.

**Current release candidate:** `v0.2.0-alpha.5.1`, by DrapNard. No compilation evidence exists yet for this archive because the generating environment had no Rust compiler and no network to install one.

A portable **Git bundle** accompanies the source ZIP. It contains a reconstructed history from v0.1.0-alpha through v0.2.0-alpha.5.1, with annotated release tags attributed to DrapNard. These are local release-package commits, **not** pre-existing GitHub commits. Release commits use the same non-deliverable GitHub-style noreply identity as the previous portable bundle; the maintainer should configure their real Git identity if desired. The maintainer should set their real Git identity for subsequent commits.

```bash
git clone Kalium-v0.2.0-alpha.5.1-git.bundle Kalium
cd Kalium
git tag --list
```

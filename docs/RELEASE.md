# Publishing Kalium releases — maintained by DrapNard

Kalium is an independent project; do not publish it under an Adobe name or imply official SPP compatibility while that format is unsupported.

1. Confirm `cargo test --all-targets`, `cargo check --all-targets`, `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` on a host with a current Rust compiler.
2. Run `cargo run -- validate examples/Kalium-Demo.kalium`, `cargo run -- export examples/Kalium-Demo.kalium ./exports-smoke`, and `cargo run -- preview examples/Kalium-Demo.kalium ./preview.png`.
3. Open the GUI, test imported PNG, OBJ and demo `.kalium`; paint, fill, undo/redo, save/reopen.
4. Update `Cargo.toml`, `README.md`, `CHANGELOG.md`, `AUTHORS.md` and `docs/ROADMAP.md` to match the release.
5. Commit and sign/tag with the maintainer's own locally configured Git identity. Do not fabricate email addresses or signatures.
6. Publish an archive plus its SHA-256; mark source-only releases honestly and avoid claiming compiled binaries unless built and tested.

**Current release candidate:** `v0.2.0-alpha.1`, by DrapNard. No compilation evidence exists yet for this archive because the generating environment had no Rust compiler and no network to install one.

A portable **Git bundle** accompanies the source ZIP. It contains a reconstructed history with a baseline `v0.1.0-alpha` snapshot and the `v0.2.0-alpha.1` iteration tagged under the author name DrapNard. These are local release-package commits, **not** pre-existing GitHub commits. Because a verified email was not available, bundle commits use the technical, non-deliverable placeholder `noreply@kalium.invalid`. The maintainer should set their real Git identity for subsequent commits.

```bash
git clone Kalium-v0.2.0-alpha.1-git.bundle Kalium
cd Kalium
git tag --list
```

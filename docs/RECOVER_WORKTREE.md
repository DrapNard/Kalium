# Fixing local unclosed delimiters after bundle upgrades

The tagged alpha.5.1 source has **250** lines in `src/document.rs`; a user report referenced line **411**, indicating a modified working copy. A `git stash apply` after merging a release can restore obsolete or malformed Rust source. A new Git bundle cannot overwrite such edits by itself.

## Safe update (existing clone in ~/Dev/Kalium)

1. Download the `Kalium-v0.2.0-alpha.5.2-git.bundle` into `~/Téléchargements`.
2. Back up tracked and untracked edits before moving the branch:

```sh
cd ~/Dev/Kalium
git status --short
git diff -- src/document.rs src/app.rs
cp src/document.rs /tmp/kalium-document-before-alpha5.2.rs
git stash push -u -m "local Kalium edits before alpha.5.2"
```

3. Fetch, fast-forward, and **do not restore your old stash yet**:

```sh
git fetch ~/Téléchargements/Kalium-v0.2.0-alpha.5.2-git.bundle \
  main:refs/remotes/kalium-bundle/main
git merge --ff-only kalium-bundle/main
git status --short
git log -1 --oneline
grep '^version' Cargo.toml
wc -l src/document.rs
cargo check --all-targets
cargo test --all-targets
```

4. If the file still fails to parse, inspect the difference **before** reverting it:

```sh
git diff -- src/document.rs
git show HEAD:src/document.rs | wc -l
wc -l src/document.rs
```

If you need to repair **only** `src/document.rs` and have already saved the local copy, use:

```sh
git restore --source=HEAD -- src/document.rs
```

Never use `git reset --hard` to solve this. Keep your `stash` until you've selectively recovered any desired changes. Do not blindly use `git stash pop` or `git stash apply` after the upgrade.

`rustc` and `cargo` are not installed in the packaging environment, so source compilation must be confirmed locally. The bundle is a portable Git repository, not a live remote.

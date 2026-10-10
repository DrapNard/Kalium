# Fix alpha.5 compilation and upgrade to alpha.5.1

**Reported problem:** `src/app.rs` has an `unclosed delimiter` around `autosave_workspace()` in a local working copy. This can happen when previously stashed modifications are applied after a Git bundle update. Alpha.5's tagged source contains a balanced function; alpha.5.1 rewrites it in readable, early-return style to avoid ambiguity.

From an existing clone (use the actual bundle filename downloaded into `~/Téléchargements`):

```bash
cd ~/Dev/Kalium
# Retain a reversible copy of tracked edits and new files.
git status --short
git stash push -u -m "local edits before Kalium alpha.5.1"
# Fetch a complete history and fast-forward only (never reset --hard).
git fetch ~/Téléchargements/Kalium-v0.2.0-alpha.5.1-git.bundle main:refs/remotes/kalium-bundle/main
git merge --ff-only kalium-bundle/main
# Verify *this release* before reapplying any saved local patches.
git log -1 --oneline
git status --short
cargo check --all-targets
cargo test --all-targets
cargo check --all-targets --features gpu-experimental
```

If the tests pass, **do not automatically `git stash pop`**: an older `src/app.rs` from the stash can reintroduce the parser error. Your stash remains available through `git stash list`. Restore only the changes you actually need after comparing them with the new version (`git stash show -p 'stash@{0}'`).

The complete source ZIP is an alternative for a clean build without touching your existing working copy.

This release has not yet been compiled or executed in the packaging environment (Rust toolchain unavailable here). If compilation fails, provide the exact new error output.

# Authorship and release ownership

**Kalium — by DrapNard.**

- Project creator, original maintainer and release identity: **DrapNard** — <https://github.com/DrapNard>
- Later contributions: their respective authors, credited through Git history and release notes.
- Code license: **AGPL-3.0-only**. Copyright © 2026 DrapNard and Kalium contributors.
- Package ID: `app.vulpyr.kalium`.

## Version scheme

Kalium follows Semantic Versioning for release numbering and uses pre-release identifiers while it is not considered production-ready:

- `v0.1.0-alpha` — first uncompiled source prototype, 2026-10-10.
- `v0.2.0-alpha.1` — DrapNard-branded second source iteration, 2026-10-10.
- `v0.2.0-alpha.2` — UV-aware painting, flood-fill and experimental compute source iteration, 2026-10-10.
- `v0.2.0-alpha.3` — source-level dockable Substance-inspired workspace, 2026-10-10.

Until v1.0, feature formats and APIs may change, but format migrations should not silently discard data.

Recommended Git commands for the maintainer after testing locally:

```bash
git init
git config user.name "DrapNard"
# Configure a real email in git config yourself; don't invent one.
git add .
git commit -m "release: Kalium v0.2.0-alpha.3"
git tag -a v0.2.0-alpha.3 -m "Kalium v0.2.0-alpha.3 by DrapNard"
```

Tagging does not imply a GitHub release has been published; no remote repository is created by this source archive.

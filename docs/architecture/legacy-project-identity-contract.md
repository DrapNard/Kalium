# Stable recovery identity for legacy projects (S5-038)

Versions 1–5 have no persisted project UUID. Loading identical legacy archive
bytes must yield the same non-nil UUID, including across process restarts and
copied paths. Versions 6–9 must retain their authored UUID exactly. Saving a
migrated project as v9 persists the assigned UUID using existing Save semantics.

Derive a domain-separated SHA-256 fingerprint from the already-open archive
descriptor, using bounded streaming (the existing 1-GiB archive limit, 64-KiB
scratch). Use its first 16 bytes as an RFC-variant UUIDv8. Hash the complete
archive and cap actual bytes read, including growth after metadata inspection.
Do not reopen the pathname to derive identity: it may have been replaced while
the original archive descriptor was being decoded. Do not modify input files.

Keep existing journal UUID+baseline-digest validation authoritative. This change
must not accept a mismatched or corrupt journal, delete rejected journal data,
or change IDs in v6+ projects. Journals created by older buggy builds with random
legacy IDs remain protected/rejected; do not guess their identity or silently
rewrite them. Record this migration limitation in the handoff.

Storage worker owns `src/project.rs` only: implement derivation and tests for
v1–v5 repeated/copy loads, changed-source identity, migration persistence and
existing explicit IDs. Parent owns app lifecycle/recovery tests and docs. Do not
run Cargo concurrently; parent serializes integration checks after review.

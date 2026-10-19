# Recent projects contract (P0-A / S12-011)

Recent projects are per-user preferences, never project content. The File menu
must reuse the normal guarded Open path: recovery guards, provisional project
locks and Save/Discard/Cancel remain authoritative. Promote a path only after
an accepted primary open or successful native project save. Recovery sources
and canceled/failed opens never enter the list. Storage errors are nonfatal.

`src/recent_projects.rs` owns `RecentProjects` with these interfaces:

- `load(path: PathBuf) -> anyhow::Result<Self>`: absent file means empty list.
- `empty(path: PathBuf) -> Self`: fallback after malformed storage.
- `entries(&self) -> &[PathBuf]`: newest first, including now-missing files.
- `record(&mut self, path: &Path) -> anyhow::Result<()>`: canonicalize an existing
  file, deduplicate and move to front, cap at 12 entries, persist transactionally.
- `remove(&mut self, path: &Path) -> anyhow::Result<()>` and
  `clear(&mut self) -> anyhow::Result<()>`: persist before replacing live state.

Version-1 JSON is capped at 32 KiB; each path at 4096 UTF-8 bytes. Stored paths
are absolute. Reject non-UTF-8 paths rather than changing them through lossy
encoding. Bound reads before JSON decoding. Keep the previous in-memory and
durable list on write failure. Use a synced same-directory temporary and atomic
replacement, including native Windows replacement when a destination exists.
Preserve unknown JSON fields when rewriting a supported version.

The app owns module registration, user-config path selection, initialization,
menu integration, lifecycle promotion and headless pointer/lifecycle tests.
Missing menu entries are disabled for opening, with explicit removal available;
recheck file existence when clicked. A storage failure must not turn an already
successful project save/open into a failure or disturb its recovery journal.

Ownership: storage worker edits only `src/recent_projects.rs`; parent edits
`src/app.rs`, `src/main.rs`, `src/layout.rs` and documentation. Review worker
changes before running the serialized integrated checks.

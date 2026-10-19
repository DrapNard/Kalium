# SPP open/save priority

Owner direction, 2026-10-19: public GitHub repository; real editable SPP open/save
is the next priority and the sole target project-format workflow. Further UI
polish is deferred. Current SPP support is detection only; this objective is NOT
achieved. Native `.kalium` remains existing alpha behavior while compatibility
is developed, and is not a substitute for the requested result.

## Acceptance

1. Inventory user-owned or independently created SPP fixtures with Painter
   version, save mode, provenance, size and digest. Keep private/proprietary
   resources outside the repository.
2. Establish the container and semantic record contract from official public
   documentation and bounded clean-room observations. Preserve unknown records
   and references; cap allocation, recursion, decompression and external links.
3. Import mesh/material bindings, Texture Sets, editable layers/channels/masks
   for the supported corpus. Report unsupported semantics explicitly.
4. Save a genuine SPP after a Kalium edit. Open it in Painter and verify that
   the project is editable; compare layer state and exported maps. Save again
   in Painter and reopen in Kalium. Include full and incremental Painter saves.
5. Prove failure safety: original files survive failed/canceled saves, unsupported
   records remain intact, malformed/oversized fixtures reject before mutation.

Detection, flattened texture extraction, a Painter-dependent bridge, renaming
Kalium archives, and unchanged-byte copying do not satisfy standalone editable
SPP support. No completion claim until the real round-trip evidence exists.

## Current investigation

- Local source audit confirms no SPP reader/writer or format fixtures.
- Official Painter APIs document open/save operations, but reviewed sources do
  not specify the full on-disk schema.
- REA Mac is connected with Ghidra available. A bounded application-name search
  located the installed Painter app. No application source/shaders/assets are
  copied. An original quad fixture now full-saves and reopens editable in Painter.
  The observed HDF5 container, document declarations and nonstandard mesh version
  are recorded in [the investigation](../research/spp-roundtrip.md). Semantic
  import, editable save and incremental round trips remain unimplemented.
- Native Save now rejects SPP paths before mutation rather than replacing an
  SPP with a mislabeled Kalium archive. This safeguard is not SPP writing.

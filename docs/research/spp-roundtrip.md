# SPP controlled fixture investigation

2026-10-19. **Research only: Kalium cannot yet open or save editable SPP projects.**
The owner has made standalone SPP editing the sole target project-format workflow.

## Reproduction and provenance

`scripts/research/painter_spp_fixture.py --painter /absolute/path/to/Painter`
uses the documented local Python endpoint with an original four-vertex quad.
It refuses an existing endpoint, verifies the remote PID before creating a
project, refuses an already open project, and terminates only its own process.
It creates a private temporary directory, full-saves a baseline and a copy,
then reopens the baseline and checks that it is editable and unchanged on disk.
Responses, file reads and startup/operation time are bounded. This is an oracle
harness, not a runtime dependency or an SPP implementation.

Observed reference: Painter **11.1.3**, macOS ARM64, Python API **0.3.5**.
Executable SHA-256:
`43c14c7a679c5bde792460dca975a801e8fd2e1978d06a4e2fb0eb987353bf08`.
Successful runtime evidence:
`ev_e2be826e33200d9e4b873e1c09c1e48feac9eaaaf1c1a5852e8266a909463314`.
REA bound the installed executable with Ghidra available; no decompiled source
was used for these findings.

| Private artifact | Bytes | SHA-256 |
|---|---:|---|
| baseline-full.spp | 12783808 | `3098ac69e4db08cea155dd67902c128f93d5e6d01d31dad8b51e6500e361e34c` |
| same-project-full-copy.spp | 12783855 | `8b89ae403298d2c8ceaf8597a753d8ab00eb61dbcffaffd690e28865fd092a75` |
| paint/document.bin | 2352 | `a376fda81cc46eef28d22da11d3d1923067b4220f6008ee740b62d11b6eb5664` |

Original OBJ SHA-256:
`456971e59144b40afecc9d7d9ab75e3ea953c7a1e821386e65226e49e060ae14`.
The original geometry is embedded in the harness. Fixtures include Painter's
bundled resource payloads and remain private: do not commit SPPs, shaders or
images. Only the owned document/mesh metadata was extracted for investigation;
resource datasets were inventoried by name/size, not extracted.

## Observations and limits

- These two full saves have an HDF5 signature at byte zero. A bounded `h5dump`
  inventory with HDF5 plugins disabled recognizes the container. This does not
  establish compatibility across Painter versions or incremental saves.
- The container separates `/paint/document.bin`, `/paint/scene.assbin`, editor
  state, project settings, resources and texture-cache datasets. Observed byte
  datasets have an `m3_x64_128` two-element U64 attribute. Its name suggests a
  hash, but algorithm, seed, coverage and byte order remain unverified.
- `document.bin` begins `dd 2f 7c 1b`. Little-endian header words include 2352,
  matching its length, and 11, matching visible `Data...` declarations. Their
  semantics remain unproven. Length-prefixed names include `DataDocument`,
  `DataMaterial`, `DataChannel`, `DataStackLayers`, `DataLayerColor`, and
  properties such as `materials`, `maskEnabled`, `perChannelBlending` and `uid`.
  These are evidence of structured state, not a decoded layer graph.
- The mesh dataset begins `ASSIMP.binary-dump.` but reports version **1.7**.
  Its payload does not contain the current upstream scene chunk identifier.
  Upstream Assimp's reader requires **1.0**; shared magic is insufficient to
  claim standard ASSBIN import. The companion original OBJ establishes the
  fixture geometry independently.
- `projectsettings.ini` is binary despite the suffix; UTF-16-like version keys
  are visible. A Qt-style encoding is a hypothesis, not an established parser.
- The public project UUID changed after close/open in this run. An initial
  harness assertion incorrectly assumed stability; it was replaced with
  recording both observations and checking the file digest. Do not infer
  persistent document identity from this API alone.
- Full-copy bytes differ from baseline bytes. Neither a byte difference nor an
  unchanged-byte copy demonstrates a writable semantic format.

## Next acceptance work

1. Generate controlled differential fixtures: one layer rename, opacity change,
   fill, mask, channel and incremental save at a time; retain oracle state.
2. Establish bounded document declarations/instances and mesh schema before
   implementing semantic import. Verify checksum hypotheses against fixtures.
3. Choose an HDF5 integration that preserves unknown objects and is buildable
   on supported platforms; reject external links and unsupported semantics.
4. Implement genuine edits and atomic save, then reopen in Painter and compare
   editable layer state and exported maps. Repeat Painter → Kalium → Painter →
   Kalium, including failure preservation. Until then SPP support is incomplete.

## Public contracts consulted

- [Painter project API](https://experienceleague.adobe.com/en/docs/substance-3d-dev/painter-python/api/substancepainter-package/project): create/open, editable state, full/incremental save modes and save-as-copy.
- [Painter remote control](https://experienceleague.adobe.com/en/docs/substance-3d-dev/painter-python/tutorials/remote-control): local scripting endpoint and request encoding.
- [Painter command lines](https://experienceleague.adobe.com/en/docs/substance-3d-painter/using/pipeline-and-integration/configuration/command-lines): launch options.
- [Assimp binary definitions](https://github.com/assimp/assimp/blob/master/code/Common/assbin_chunks.h) and [loader](https://github.com/assimp/assimp/blob/master/code/AssetLib/Assbin/AssbinLoader.cpp): comparison against the public 1.0 contract, not a schema for the observed 1.7 payload.

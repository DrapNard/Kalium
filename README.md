# Kalium — exploratory integration branch

> This branch records the initial Kalium-based port experiment. The owner
> subsequently selected **Subcraft as the main app**. Continued integration is
> in DrapNard/subcraft; this branch is not a validated merged release.

**An open-source 3D texture painter, written in Rust.**

[Source](https://github.com/DrapNard/Kalium) · [Roadmap](docs/ROADMAP.md) ·
[Compatibility](docs/COMPATIBILITY.md) · [Contributing](CONTRIBUTING.md)

Kalium combines its existing editor and project-safety work with selected
open-source rendering, compositing and baking code from
[SubCraft](https://github.com/DrapNard/subcraft). It keeps one native application,
Kalium branding, and the AGPL-3.0-only license. Incorporated SubCraft portions
retain their MIT notices and contributor credits.

**Status: `0.3.0-alpha.2`, source alpha.** This is an actively developed painter,
not a finished production release. Read the limitations below before using it
for important work.

## Priority: editable SPP projects

The target project workflow is **open, edit and save real Substance Painter
`.spp` files locally**. This is **not implemented yet**. Current editable saves
use Kalium's own `.kalium` format. Native Save refuses `.spp` filenames to avoid
replacing a Painter project with a mislabeled archive.

A private, independently created Painter fixture corpus now covers full saves,
incremental saves, layer renames, opacity and visibility, with successful
Painter reopen checks. The semantic reader and writer remain under development.
Detection, flattened textures and a Painter-dependent bridge do not meet the
acceptance target. See [SPP plan](docs/roadmap/spp-focus.md) and
[research evidence](docs/research/spp-roundtrip.md).

## What the combined app offers

- **Painting:** direct 3D surface and 2D UV painting; brush, eraser, fill, masks,
  alpha tips, editable brush presets, UV clipping and sparse stroke history.
- **Materials:** eight editable channels: Base Color, Roughness, Metallic,
  Normal, Height, Emissive, Opacity and Ambient Occlusion.
- **Layers:** recursive Paint, Fill and Group trees, masks, visibility, opacity,
  channel switches, procedural Roughness and an expanded blend-mode family
  adapted from SubCraft, including arithmetic, contrast, HSV and normal modes.
- **Viewport:** an experimental GPU preview with SubCraft-derived GGX shading,
  procedural studio reflections and tone mapping. All eight channel textures
  have incremental uploads. The CPU renderer remains the reference for
  rendering, picking and painting.
- **Baking:** world normals, position, material ID, AO, high-to-low world-normal
  projection, plus SubCraft-derived 3D-neighbourhood curvature and inward-ray
  thickness through the CLI. Bake outputs are standalone maps.
- **Projects:** multiple Texture Sets and material bindings; bounded native
  archive loading, unknown-field preservation, undo/redo, recovery journals and
  checkpoints. Workspace preferences stay outside project files.
- **Interchange:** OBJ, glTF and GLB mesh import; PNG/JPEG texture import; PNG
  PBR exports and packed engine profiles. Format limits are documented in the
  compatibility matrix.
- **Workspace:** compact dark docks for Assets, 3D/UV, Texture Sets, Layers,
  Properties and Brush Studio, with resizable and restorable panes.

The [integration record](docs/architecture/subcraft-integration.md) explains
which implementation was retained in each area and what still needs adapters.
This is a source integration, not a claim that every SubCraft feature is present.

## Build and run

Use a current stable Rust toolchain and the desktop libraries required by
eframe/wgpu. Kalium source uses Rust 2021; Kalcite is not required.

```bash
git clone https://github.com/DrapNard/Kalium.git
cd Kalium
cargo run --release
```

On Debian/Ubuntu, the CI build installs:

```bash
sudo apt-get install pkg-config libxkbcommon-dev libwayland-dev libx11-dev \
  libxcursor-dev libxrandr-dev libxi-dev libasound2-dev libudev-dev
```

The GPU viewport uses the normal application build. `gpu-experimental` enables
an additional compute-brush prototype; it does not enable GPU painting in the
editor.

### First session

1. Run Kalium and use the demo cube, or import `examples/TwoMaterials.obj`.
2. Paint in the 3D view or select the 2D/UV tab. Right-drag orbits; scroll zooms.
3. Add Paint, Fill or Group layers and choose a material channel.
4. Save a `.kalium` project and export PBR maps from File.

Use **Window → Restore default workspace** to apply the current layout to an
existing installation. Closed panes can be restored from Window. The Assets
panel contains implemented presets and import actions, not Adobe's asset library.

## Command line

```bash
cargo run --release -- --help
cargo run --release -- validate examples/Kalium-Demo.kalium
cargo run --release -- preview examples/Kalium-Demo.kalium preview.png
cargo run --release -- export-all examples/Kalium-Demo.kalium ./textures
cargo run --release -- bake-curvature examples/Kalium-Demo.kalium curvature.png 1 16 0.1
cargo run --release -- bake-thickness examples/Kalium-Demo.kalium thickness.png 1 16 1.0 42
```

The last bake arguments are Texture Set ID, samples, world-space radius/distance,
and optional seed. Curvature uses the radius; thickness uses the sample count
and seed. Curvature is gray at flat surfaces, bright at convex areas and dark
at concave areas. Thickness is bright for nearby opposite surfaces; open areas
without a hit in range are dark. Uncovered UV texels are transparent.

## Limits and verification

- SPP read/write, SBSAR execution and Substance preset compatibility are missing.
- GPU picking/painting are not implemented. The GPU preview uses fixed studio
  lighting, derivative tangent frames, height bump rather than displacement,
  and binary opacity cutout rather than layered transparency. CPU/GPU parity
  and broad hardware acceptance remain open.
- No UDIM workflow, tablet pressure/tilt integration or complete smart-material
  and effects library. SubCraft's pen paths, parts workflow, HDRI pipeline,
  tiled float storage and projection tools are not yet integrated.
- Native projects currently use manifest v9; versions 1–8 migrate on load.
  Older application builds cannot read v9. Keep backups before resaving.
- Importers enforce explicit limits and reject unsupported deformation/UV data.
  See the compatibility matrix for exact scope.
- UI screenshot fidelity remains in progress; compilation does not establish
  visual completion. Hosted CI and platform results must be checked separately.

The [test matrix](docs/qa/test-matrix.md),
[implementation status](docs/roadmap/implementation-status.md) and
[visual review](docs/visual/painter-classic-status.md) distinguish inspected,
tested and unverified behavior. To run the development gates:

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --all-targets --features gpu-experimental
```

## License and credits

Kalium is **AGPL-3.0-only**, maintained by DrapNard and contributors. Adapted
SubCraft code credits Aalzard and the SubCraft contributors under MIT; see
[third-party notices](THIRD_PARTY_NOTICES.md). No Adobe source, proprietary
shaders, icons or bundled resources are included by this integration.

The official product includes no generative AI image or material features.
AI-assisted code development is permitted with review. See the
[artist-first charter](PHILOSOPHY.md).

Kalium is independent and is not affiliated with or endorsed by Adobe.

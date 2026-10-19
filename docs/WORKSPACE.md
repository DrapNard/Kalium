# Kalium UI / workspace architecture

The `2026-10-10` Substance Painter screenshot is **design reference only**. It is not imported or modified, and no image-generation workflow is used to implement the app. The official Kalium interface is implemented with real egui widgets.

## Layout

`src/layout.rs` declares one `Pane` enumeration (8 tabs), their names, a default docking tree, loading and saving. `src/app.rs` implements `egui_dock::TabViewer` and maps each pane to application methods. `egui_dock` 0.16 targets `egui` / `eframe` 0.31. The workspace is a per-user preference stored outside `.kalium` project files.

- Assets: search bundled and user-local brush presets; refresh the shelf; import bounded `.kbrush` files into the per-user shelf; import textures as layers. The shelf lives in `brush-presets` beside the workspace config, scans one directory level (512-entry / 128-preset caps), and skips invalid preset files. Synthetic smart materials are not implemented.
- 3D Viewport: mesh view, hit testing, orbit, sampling and painting.
- 2D / UV: UV atlas, triangulated wire overlay, painting and sampling.
- Texture Set List: create, select, rename and remove multiple sets; bind mesh material slots to sets and optionally switch sets when painting a material.
- Layers: editable stack with add, duplicate, remove, reorder, visibility, opacity, blend modes, channel enablement and masks.
- Texture Set Settings: set resolution and UV paint clipping.
- Properties: channel selection, brush color/intensity, size/opacity, fill, mask and display controls.
- Brush Studio: brush modes and controls, local preset load/save, display settings and texture resolution controls.

The layout file and brush preset shelf are saved in the user configuration directory, **outside the project file**. The user can move tabs around or close them, then reopen individual panels from the View menu or restore the default layout. On a malformed workspace file, Kalium falls back to the default layout. Workspace serialization, default-pane coverage and local preset import/search/selection have automated tests; desktop interaction, persisted positions and visual behavior still require runtime acceptance.

## Why `egui_dock`?

The user explicitly asked for independently moveable, detachable, resizable, closable panels. Plain fixed `SidePanel`s are not suitable. `egui_dock` provides a flexible docking tree and detachable windows, while allowing Kalium to keep each pane's actual implementation in Rust.

## Build / verification

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo run --release
```

1. [ ] Confirm both 3D and UV viewports render at launch.
2. [ ] Drag `Layers` into the left column, then move it into a floating window.
3. [ ] Resize panes; close `Assets`, then re-open via `View > Assets`.
4. [ ] Restart Kalium; verify persisted layout and window positions.
5. [ ] Reset via `View > Restore default workspace`.
6. [ ] Paint using 3D then UV views; save and load a `.kalium` document.
7. [ ] Run UI in X11/Wayland, Windows and macOS separately.
8. [ ] Import a `.kbrush` in Assets, search for it, select it, then restart and confirm it remains in the local shelf.

Source-level workspace tests and headless UI interaction tests cover serialization, pane availability and selected editor workflows. The desktop visual/runtime checklist above remains unchecked; no successful desktop acceptance is claimed here.

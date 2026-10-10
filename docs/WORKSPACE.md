# Kalium UI / workspace architecture

The `2026-10-10` Substance Painter screenshot is **design reference only**. It is not imported or modified, and no image-generation workflow is used to implement the app. The official Kalium interface is implemented with real egui widgets.

## Layout

`src/layout.rs` declares one `Pane` enumeration (8 tabs), their names, a default docking tree, loading and saving. `src/app.rs` implements `egui_dock::TabViewer` and maps each pane to application methods. `egui_dock` 0.16 targets `egui` / `eframe` 0.31.

- Assets: load texture, brushes and local presets. No synthetic smart materials.
- 3D: mesh view, hit-test, orbit, sampling and paint.
- 2D: UV atlas and triangulated wire overlay, painting and sampling.
- Texture Set List: actual single supported texture set (multi-set workflow pending).
- Layers: existing editable stack, mask settings and blend modes.
- Texture Set Settings: atlas size and clipping.
- Properties: channel, brush color/intensity, size/opacity and fill operation.
- Brush Studio: existing brush controls and display settings.

The layout file is saved in the user configuration directory, **outside the project file**. The user can move tabs around or close them, then reopen from the View menu. On a malformed workspace file, Kalium falls back to the default layout.

## Why `egui_dock`?

The user explicitly asked for independently moveable, detachable, resizable, closable panels. Plain fixed `SidePanel`s are not suitable. `egui_dock` provides a flexible docking tree and detachable windows, while allowing Kalium to keep each pane's actual implementation in Rust.

## Build / verification

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo run --release
```

1. Confirm both 3D and UV viewports render at launch.
2. Drag `Layers` into the left column, then move it into a floating window.
3. Resize panes; close `Assets`, then re-open via `View > Assets`.
4. Restart Kalium; verify persisted layout and window positions.
5. Reset via `View > Restore default workspace`.
6. Paint using 3D then UV views; save and load a `.kalium` document.
7. Run UI in X11/Wayland, Windows and macOS separately.

No compilation/runtime/visual tests were possible during source generation. This checklist must not be marked passed before running it.

//! Native, user-editable workspaces. Panels are data, never hard-coded sidebars.
//! Layout state is stored per user, separately from artistic document content.
use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_WORKSPACE_BYTES: usize = 1024 * 1024;
const WORKSPACE_SCHEMA_VERSION: u64 = 1;
const WORKSPACE_SCHEMA_FIELD: &str = "kalium_workspace_version";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Pane {
    Assets,
    Viewport3D,
    Viewport2D,
    TextureSets,
    Layers,
    TextureSettings,
    Properties,
    BrushStudio,
    Activity,
}

impl Pane {
    pub const ALL: [Self; 9] = [
        Self::Assets,
        Self::Viewport3D,
        Self::Viewport2D,
        Self::TextureSets,
        Self::Layers,
        Self::TextureSettings,
        Self::Properties,
        Self::BrushStudio,
        Self::Activity,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Assets => "ASSETS",
            Self::Viewport3D => "3D VIEW",
            Self::Viewport2D => "2D / UV",
            Self::TextureSets => "TEXTURE SET LIST",
            Self::Layers => "LAYERS",
            Self::TextureSettings => "TEXTURE SET SETTINGS",
            Self::Properties => "PROPERTIES",
            Self::BrushStudio => "BRUSH STUDIO",
            Self::Activity => "ACTIVITY",
        }
    }
}

pub fn default_workspace() -> DockState<Pane> {
    use Pane::*;
    // Painter's main canvas and UV view are tabs in one leaf. Keep the 3D
    // canvas selected on first launch.
    let mut dock = DockState::new(vec![Viewport3D, Viewport2D]);
    let tree = dock.main_surface_mut();
    // With 38 px tool rails on both sides, a 0.1075 left split gives Assets
    // about 198 px at the 1920 px reference width.
    let [center, _left] = tree.split_left(NodeIndex::root(), 0.1075, vec![Assets]);
    // Leave about 19% of the remaining width for the inspector stack.
    let [_center, right] = tree.split_right(center, 0.81, vec![TextureSets]);
    // Texture Sets occupy the top 18.5% of the right column. Layers and Texture
    // Settings share the middle leaf; Properties gets the remaining 58% below.
    let [_sets, layers] = tree.split_below(right, 0.185, vec![Layers, TextureSettings]);
    tree.split_below(layers, 0.42, vec![Properties, BrushStudio, Activity]);
    dock
}

/// Show or hide one dock panel without touching project state. A hidden panel
/// can be restored later through the Window menu.
pub fn set_pane_visible(dock: &mut DockState<Pane>, pane: Pane, visible: bool) -> bool {
    let currently_visible = dock.find_tab(&pane).is_some();
    if currently_visible == visible {
        return false;
    }
    if visible {
        dock.push_to_first_leaf(pane);
    } else {
        let Some(location) = dock.find_tab(&pane) else {
            return false;
        };
        if dock.remove_tab(location) != Some(pane) {
            return false;
        }
    }
    true
}

/// Reveal a dock pane if needed, select its tab, and focus its leaf.
/// This changes workspace UI state only; it does not access project content.
pub fn focus_pane(dock: &mut DockState<Pane>, pane: Pane) {
    if dock.find_tab(&pane).is_none() {
        set_pane_visible(dock, pane, true);
    }
    if let Some(location) = dock.find_tab(&pane) {
        dock.set_active_tab(location);
        dock.set_focused_node_and_surface((location.0, location.1));
    }
}

pub fn workspace_file() -> Option<PathBuf> {
    // Respect the platform/user configuration directory without requiring another crate.
    let base = if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(xdg)
    } else if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)?
    } else {
        PathBuf::from(env::var_os("HOME")?).join(".config")
    };
    Some(base.join("kalium").join("workspace-v1.json"))
}

/// Per-user directory for native brush presets, separate from project files.
pub fn brush_shelf_directory() -> Option<PathBuf> {
    workspace_file().map(|workspace| workspace.with_file_name("brush-presets"))
}

/// Bounded user activity history, independent from project content and recovery.
pub fn activity_log_file() -> Option<PathBuf> {
    workspace_file().map(|workspace| workspace.with_file_name("activity-v1.json"))
}

/// Recently accepted native project paths, independent from project content.
pub fn recent_projects_file() -> Option<PathBuf> {
    workspace_file().map(|workspace| workspace.with_file_name("recent-projects-v1.json"))
}

/// Per-user checkpoint location, separate from the selected project and its
/// transactional `<project>.tmp` save file.
pub fn recovery_checkpoint_file() -> Option<PathBuf> {
    workspace_file().map(|workspace| workspace.with_file_name("recovery-v1.kalium"))
}

/// Crash-recovery baseline for the current unsaved editor session. The project
/// format keeps this separate from both workspace preferences and user files.
pub fn autosave_session_file() -> Option<PathBuf> {
    workspace_file().map(|workspace| workspace.with_file_name("autosave-session-v1.kalium"))
}

pub fn load_workspace() -> DockState<Pane> {
    workspace_file()
        .map(|path| load_workspace_at(&path))
        .unwrap_or_else(default_workspace)
}

fn load_workspace_at(path: &Path) -> DockState<Pane> {
    let result = (|| -> std::io::Result<Vec<u8>> {
        let file = fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(MAX_WORKSPACE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    result
        .map(|bytes| load_workspace_from_bytes(&bytes))
        .unwrap_or_else(|_| default_workspace())
}

fn load_workspace_from_bytes(bytes: &[u8]) -> DockState<Pane> {
    if bytes.len() > MAX_WORKSPACE_BYTES {
        return default_workspace();
    }
    decode_workspace(bytes).unwrap_or_else(|_| default_workspace())
}

// New egui_dock nodes use Rect::NOTHING, whose x/y coordinates are infinite.
// serde_json turns these infinities into null; deserializing that as f32 fails.
// Repair only coordinate *objects* (the {min:{x,y},max:{x,y}} shape), not
// meaningful null Option<T> fields such as focused_node or window state.
// This also repairs workspace JSON written by earlier Kalium alphas.
fn normalize_rectangles(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("min") && fields.contains_key("max") {
                for corner in ["min", "max"] {
                    if let Some(Value::Object(coords)) = fields.get_mut(corner) {
                        if coords.contains_key("x") && coords.contains_key("y") {
                            for axis in ["x", "y"] {
                                if coords.get(axis).is_some_and(Value::is_null) {
                                    coords.insert(axis.to_owned(), Value::from(0.0));
                                }
                            }
                        }
                    }
                }
            }
            for subvalue in fields.values_mut() {
                normalize_rectangles(subvalue);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_rectangles(item);
            }
        }
        _ => {}
    }
}

pub fn encode_workspace(state: &DockState<Pane>) -> anyhow::Result<Vec<u8>> {
    let mut json = serde_json::to_value(state)?;
    let fields = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("serialized workspace is not an object"))?;
    fields.insert(
        WORKSPACE_SCHEMA_FIELD.to_owned(),
        Value::from(WORKSPACE_SCHEMA_VERSION),
    );
    normalize_rectangles(&mut json);
    Ok(serde_json::to_vec_pretty(&json)?)
}

pub fn decode_workspace(bytes: &[u8]) -> anyhow::Result<DockState<Pane>> {
    let mut json: Value = serde_json::from_slice(bytes)?;
    if let Some(fields) = json.as_object_mut() {
        if let Some(version) = fields.remove(WORKSPACE_SCHEMA_FIELD) {
            anyhow::ensure!(
                version.as_u64() == Some(WORKSPACE_SCHEMA_VERSION),
                "unsupported workspace schema version"
            );
        }
    }
    normalize_rectangles(&mut json);
    Ok(serde_json::from_value(json)?)
}

pub fn persist_workspace(state: &DockState<Pane>) -> anyhow::Result<Vec<u8>> {
    if let Some(path) = workspace_file() {
        persist_workspace_at(&path, state)
    } else {
        encode_workspace(state)
    }
}

/// Persist a workspace to an explicit path. The editor normally uses the
/// per-user path above; this entry point also supports isolated lifecycle tests.
pub fn persist_workspace_at(path: &Path, state: &DockState<Pane>) -> anyhow::Result<Vec<u8>> {
    let encoded = encode_workspace(state)?;
    persist_workspace_bytes(path, &encoded, replace_workspace_file)?;
    Ok(encoded)
}

fn persist_workspace_bytes<F>(path: &Path, encoded: &[u8], replace: F) -> anyhow::Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;

        let mut file = fs::File::create(&tmp)?;
        file.write_all(encoded)?;
        file.sync_all()?;
        drop(file);
        replace(&tmp, path)?;
        sync_workspace_parent(path)
    })();
    if let Err(error) = result {
        // Replacement failures leave the old workspace in place; remove the
        // completed temporary file so the next save can reuse its name.
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

/// Replace within the same directory to stay on one volume. On Windows,
/// ReplaceFileW replaces an existing layout atomically instead of opening a
/// window where the user's previous workspace has been deleted.
fn replace_workspace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        windows_replace_workspace_file(source, destination)
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::rename(source, destination)
    }
}

#[cfg(windows)]
fn windows_replace_workspace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    fn wide(path: &OsStr) -> Vec<u16> {
        path.encode_wide().chain(std::iter::once(0)).collect()
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source_wide = wide(source.as_os_str());
    let destination_wide = wide(destination.as_os_str());
    let succeeded = unsafe {
        if destination.exists() {
            ReplaceFileW(
                destination_wide.as_ptr(),
                source_wide.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } else {
            MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn sync_workspace_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_workspace_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir()
            .join(format!(
                "kalium-layout-{name}-{}-{nonce}",
                std::process::id()
            ))
            .join("workspace.json")
    }

    #[test]
    fn default_workspace_has_all_panels_once() {
        let dock = default_workspace();
        for pane in Pane::ALL {
            assert!(dock.find_tab(&pane).is_some(), "{}", pane.label());
        }
    }

    #[test]
    fn every_panel_can_be_hidden_and_restored_after_workspace_roundtrip() {
        let mut dock = default_workspace();
        for pane in Pane::ALL {
            let changed = set_pane_visible(&mut dock, pane, false);
            assert!(changed, "{}", pane.label());
            assert!(dock.find_tab(&pane).is_none(), "{}", pane.label());
            assert!(!set_pane_visible(&mut dock, pane, false));
        }

        let hidden = decode_workspace(&encode_workspace(&dock).unwrap()).unwrap();
        for pane in Pane::ALL {
            assert!(hidden.find_tab(&pane).is_none(), "{}", pane.label());
        }

        for pane in Pane::ALL {
            assert!(set_pane_visible(&mut dock, pane, true));
            assert!(dock.find_tab(&pane).is_some(), "{}", pane.label());
            assert!(!set_pane_visible(&mut dock, pane, true));
        }
        let restored = decode_workspace(&encode_workspace(&dock).unwrap()).unwrap();
        for pane in Pane::ALL {
            assert!(restored.find_tab(&pane).is_some(), "{}", pane.label());
        }
    }

    #[test]
    fn an_intentionally_empty_workspace_loads_and_can_restore_a_pane() {
        let mut dock = default_workspace();
        for pane in Pane::ALL {
            assert!(set_pane_visible(&mut dock, pane, false));
        }
        let bytes = encode_workspace(&dock).unwrap();
        let path = test_workspace_path("empty-dock");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();

        let mut loaded = load_workspace_at(&path);
        for pane in Pane::ALL {
            assert!(loaded.find_tab(&pane).is_none(), "{}", pane.label());
        }
        assert!(set_pane_visible(&mut loaded, Pane::Layers, true));
        assert!(loaded.find_tab(&Pane::Layers).is_some());
        for pane in Pane::ALL.into_iter().filter(|pane| *pane != Pane::Layers) {
            assert!(loaded.find_tab(&pane).is_none(), "{}", pane.label());
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_oversized_and_unsupported_workspaces_fall_back_to_defaults() {
        let malformed = load_workspace_from_bytes(b"not json");
        assert!(Pane::ALL
            .iter()
            .all(|pane| malformed.find_tab(pane).is_some()));

        let oversized_bytes = vec![b' '; MAX_WORKSPACE_BYTES + 1];
        let oversized = load_workspace_from_bytes(&oversized_bytes);
        assert!(Pane::ALL
            .iter()
            .all(|pane| oversized.find_tab(pane).is_some()));

        let mut future = serde_json::to_value(default_workspace()).unwrap();
        future[WORKSPACE_SCHEMA_FIELD] = Value::from(WORKSPACE_SCHEMA_VERSION + 1);
        let unsupported = serde_json::to_vec(&future).unwrap();
        let unsupported = load_workspace_from_bytes(&unsupported);
        assert!(Pane::ALL
            .iter()
            .all(|pane| unsupported.find_tab(pane).is_some()));
    }

    #[test]
    fn default_workspace_uses_painter_style_panel_columns() {
        let workspace = default_workspace();
        let tree = workspace.main_surface();
        let leaf_for = |pane| {
            tree.iter()
                .find(|node| node.tabs().is_some_and(|tabs| tabs.contains(&pane)))
                .unwrap()
        };
        let fractions = tree.iter().fold((Vec::new(), Vec::new()), |mut acc, node| {
            match node {
                egui_dock::Node::Horizontal { fraction, .. } => acc.0.push(*fraction),
                egui_dock::Node::Vertical { fraction, .. } => acc.1.push(*fraction),
                _ => {}
            }
            acc
        });
        let has_fraction = |values: &[f32], expected: f32| {
            values.iter().any(|value| (*value - expected).abs() < 0.001)
        };
        assert!(has_fraction(&fractions.0, 0.1075)); // Assets column
        assert!(has_fraction(&fractions.0, 0.81)); // canvas width
        assert!(has_fraction(&fractions.1, 0.185)); // Texture Set List height
        assert!(has_fraction(&fractions.1, 0.42)); // Layers section height

        let canvas = leaf_for(Pane::Viewport3D);
        assert_eq!(
            canvas.tabs(),
            Some(&[Pane::Viewport3D, Pane::Viewport2D][..])
        );
        if let egui_dock::Node::Leaf { active, .. } = canvas {
            assert_eq!(active.0, 0); // 3D is initially selected.
        } else {
            panic!("canvas panes should share one leaf");
        }
        assert_eq!(leaf_for(Pane::Assets).tabs(), Some(&[Pane::Assets][..]));
        assert_eq!(
            leaf_for(Pane::TextureSets).tabs(),
            Some(&[Pane::TextureSets][..])
        );
        assert_eq!(
            leaf_for(Pane::Layers).tabs(),
            Some(&[Pane::Layers, Pane::TextureSettings][..])
        );
        assert_eq!(
            leaf_for(Pane::Properties).tabs(),
            Some(&[Pane::Properties, Pane::BrushStudio, Pane::Activity][..])
        );

        let mut occurrences = [0; Pane::ALL.len()];
        for pane in tree.tabs() {
            let index = Pane::ALL
                .iter()
                .position(|candidate| candidate == pane)
                .unwrap();
            occurrences[index] += 1;
        }
        assert!(occurrences.into_iter().all(|count| count == 1));
    }

    #[test]
    fn focus_pane_selects_existing_tab_without_rearranging_workspace() {
        let mut dock = default_workspace();
        let original_locations = Pane::ALL.map(|pane| dock.find_tab(&pane).unwrap().1);

        focus_pane(&mut dock, Pane::Viewport2D);

        for (index, pane) in Pane::ALL.into_iter().enumerate() {
            assert_eq!(
                dock.find_tab(&pane).unwrap().1,
                original_locations[index],
                "focusing a pane should not move {}",
                pane.label()
            );
        }
        let (surface, node, _) = dock.find_tab(&Pane::Viewport2D).unwrap();
        match &dock[surface][node] {
            egui_dock::Node::Leaf { tabs, active, .. } => {
                assert_eq!(tabs[active.0], Pane::Viewport2D);
            }
            _ => panic!("focused pane should remain in a leaf"),
        }
    }

    #[test]
    fn focus_pane_restores_and_selects_hidden_tab() {
        let mut dock = default_workspace();
        assert!(set_pane_visible(&mut dock, Pane::TextureSettings, false));

        focus_pane(&mut dock, Pane::TextureSettings);

        for pane in Pane::ALL {
            assert!(dock.find_tab(&pane).is_some(), "{}", pane.label());
        }
        let (surface, node, _) = dock.find_tab(&Pane::TextureSettings).unwrap();
        match &dock[surface][node] {
            egui_dock::Node::Leaf { tabs, active, .. } => {
                assert_eq!(tabs[active.0], Pane::TextureSettings);
            }
            _ => panic!("restored pane should be in a leaf"),
        }
    }

    #[test]
    fn workspace_json_roundtrips() {
        let dock = default_workspace();
        let bytes = encode_workspace(&dock).unwrap();
        let decoded = decode_workspace(&bytes).unwrap();
        let json = String::from_utf8(bytes).unwrap();
        // None is a legal JSON null (e.g. focused_node), not a corrupted f32.
        assert!(!json.contains("\"x\": null"));
        assert!(!json.contains("\"y\": null"));
        for pane in Pane::ALL {
            assert_eq!(
                dock.find_tab(&pane).is_some(),
                decoded.find_tab(&pane).is_some()
            );
        }
    }
    #[test]
    fn not_yet_displayed_and_floating_panels_serialize() {
        let mut dock = default_workspace();
        let window = dock.add_window(vec![Pane::BrushStudio]);
        dock.get_window_state_mut(window)
            .unwrap()
            .set_position(eframe::egui::pos2(320.0, 155.0))
            .set_size(eframe::egui::vec2(440.0, 290.0));
        let bytes = encode_workspace(&dock).unwrap();
        let decoded = decode_workspace(&bytes).unwrap();
        assert!(decoded.find_tab(&Pane::Viewport3D).is_some());
        assert!(decoded.find_tab(&Pane::Assets).is_some());
        assert!(String::from_utf8(bytes).unwrap().contains("320.0"));
    }

    #[test]
    fn old_workspace_with_null_geometry_is_repaired_without_erasing_options() {
        let dock = default_workspace();
        let original = serde_json::to_vec(&dock).unwrap();
        let repaired = decode_workspace(&original).unwrap();
        for pane in Pane::ALL {
            assert_eq!(
                repaired.find_tab(&pane).is_some(),
                dock.find_tab(&pane).is_some()
            );
        }
        let sample = serde_json::json!({"rect":{"min":{"x":null,"y":3.0},"max":{"x":4.0,"y":null}},"focused_node":null});
        let mut normalized = sample;
        normalize_rectangles(&mut normalized);
        assert!(normalized["focused_node"].is_null());
        assert_eq!(normalized["rect"]["min"]["x"], 0.0);
        assert_eq!(normalized["rect"]["max"]["y"], 0.0);
    }

    #[test]
    fn replacement_failure_preserves_previous_workspace() {
        let path = test_workspace_path("failed-replace");
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::write(&path, b"previous workspace").unwrap();

        let result = persist_workspace_bytes(&path, b"new workspace", |_, _| {
            Err(std::io::Error::other("injected replacement failure"))
        });

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"previous workspace");
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn successful_replacement_updates_existing_workspace() {
        let path = test_workspace_path("successful-replace");
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::write(&path, b"previous workspace").unwrap();

        persist_workspace_bytes(&path, b"new workspace", replace_workspace_file).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new workspace");
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_dir_all(parent).unwrap();
    }
}

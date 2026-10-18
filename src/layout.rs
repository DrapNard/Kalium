//! Native, user-editable workspaces. Panels are data, never hard-coded sidebars.
//! Layout state is stored per user, separately from artistic document content.
use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, fs, path::PathBuf};

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
}

impl Pane {
    pub const ALL: [Self; 8] = [
        Self::Assets,
        Self::Viewport3D,
        Self::Viewport2D,
        Self::TextureSets,
        Self::Layers,
        Self::TextureSettings,
        Self::Properties,
        Self::BrushStudio,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Assets => "Assets",
            Self::Viewport3D => "3D Viewport",
            Self::Viewport2D => "2D / UV",
            Self::TextureSets => "Texture Set List",
            Self::Layers => "Layers",
            Self::TextureSettings => "Texture Set Settings",
            Self::Properties => "Properties",
            Self::BrushStudio => "Brush Studio",
        }
    }
}

pub fn default_workspace() -> DockState<Pane> {
    use Pane::*;
    let mut dock = DockState::new(vec![Viewport3D]);
    let tree = dock.main_surface_mut();
    let [center, _left] = tree.split_left(NodeIndex::root(), 0.86, vec![Assets, BrushStudio]);
    // Right column is split from the whole workspace, not from one viewport.
    let [center, right] = tree.split_right(center, 0.79, vec![TextureSets]);
    // 3D and UV editors coexist at the center.
    let [_model, _uv] = tree.split_right(center, 0.50, vec![Viewport2D]);
    // Right column: texture sets, layers and editable fill/brush properties.
    let [_top, bottom] = tree.split_below(right, 0.30, vec![Layers]);
    tree.split_below(bottom, 0.55, vec![Properties, TextureSettings]);
    dock
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

/// Per-user checkpoint location, separate from the selected project and its
/// transactional `<project>.tmp` save file.
pub fn recovery_checkpoint_file() -> Option<PathBuf> {
    workspace_file().map(|workspace| workspace.with_file_name("recovery-v1.kalium"))
}

pub fn load_workspace() -> DockState<Pane> {
    workspace_file()
        .and_then(|p| fs::read(p).ok())
        .and_then(|data| decode_workspace(&data).ok())
        .filter(|state| Pane::ALL.iter().any(|pane| state.find_tab(pane).is_some()))
        .unwrap_or_else(default_workspace)
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
    normalize_rectangles(&mut json);
    Ok(serde_json::to_vec_pretty(&json)?)
}

pub fn decode_workspace(bytes: &[u8]) -> anyhow::Result<DockState<Pane>> {
    let mut json: Value = serde_json::from_slice(bytes)?;
    normalize_rectangles(&mut json);
    Ok(serde_json::from_value(json)?)
}

pub fn persist_workspace(state: &DockState<Pane>) -> anyhow::Result<Vec<u8>> {
    let encoded = encode_workspace(state)?;
    if let Some(path) = workspace_file() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &encoded)?;
        // A rename ensures that a crash cannot truncate the previous layout.
        // Windows does not replace an existing file with std::fs::rename.
        if cfg!(windows) && path.exists() {
            fs::remove_file(&path)?;
        }
        if let Err(error) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_workspace_has_all_panels_once() {
        let dock = default_workspace();
        for pane in Pane::ALL {
            assert!(dock.find_tab(&pane).is_some(), "{}", pane.label());
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
}

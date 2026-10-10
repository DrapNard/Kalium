//! Native, user-editable workspaces. Panels are data, never hard-coded sidebars.
//! Layout state is stored per user, separately from artistic document content.
use std::{env,fs,path::PathBuf};
use egui_dock::{DockState,NodeIndex};
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub enum Pane {
    Assets,Viewport3D,Viewport2D,TextureSets,Layers,TextureSettings,Properties,BrushStudio,
}

impl Pane {
    pub const ALL: [Self;8]=[
        Self::Assets,Self::Viewport3D,Self::Viewport2D,Self::TextureSets,
        Self::Layers,Self::TextureSettings,Self::Properties,Self::BrushStudio,
    ];
    pub fn label(self)->&'static str {
        match self {
            Self::Assets=>"Assets",Self::Viewport3D=>"3D Viewport",Self::Viewport2D=>"2D / UV",
            Self::TextureSets=>"Texture Set List",Self::Layers=>"Layers",
            Self::TextureSettings=>"Texture Set Settings",Self::Properties=>"Properties",
            Self::BrushStudio=>"Brush Studio",
        }
    }
}

pub fn default_workspace()->DockState<Pane> {
    use Pane::*;
    let mut dock=DockState::new(vec![Viewport3D]);
    let tree=dock.main_surface_mut();
    let [center,_left]=tree.split_left(NodeIndex::root(),0.80,vec![Assets,BrushStudio]);
    // Right column is split from the whole workspace, not from one viewport.
    let [center,right]=tree.split_right(center,0.77,vec![TextureSets]);
    // 3D and UV editors coexist at the center.
    let [_model,_uv]=tree.split_right(center,0.50,vec![Viewport2D]);
    // Right column: texture sets, layers and editable fill/brush properties.
    let [_top,bottom]=tree.split_below(right,0.30,vec![Layers]);
    tree.split_below(bottom,0.55,vec![Properties,TextureSettings]);
    dock
}

pub fn workspace_file()->Option<PathBuf> {
    // Respect the platform/user configuration directory without requiring another crate.
    let base=if let Some(xdg)=env::var_os("XDG_CONFIG_HOME").filter(|s|!s.is_empty()) {
        PathBuf::from(xdg)
    } else if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)?
    } else {
        PathBuf::from(env::var_os("HOME")?).join(".config")
    };
    Some(base.join("kalium").join("workspace-v1.json"))
}

pub fn load_workspace()->DockState<Pane> {
    workspace_file().and_then(|p|fs::read(p).ok())
        .and_then(|data|serde_json::from_slice::<DockState<Pane>>(&data).ok())
        .filter(|state|Pane::ALL.iter().any(|pane|state.find_tab(pane).is_some()))
        .unwrap_or_else(default_workspace)
}

pub fn persist_workspace(state:&DockState<Pane>)->anyhow::Result<Vec<u8>> {
    let encoded=serde_json::to_vec_pretty(state)?;
    if let Some(path)=workspace_file() {
        if let Some(parent)=path.parent() {fs::create_dir_all(parent)?;}
        let tmp=path.with_extension("json.tmp");
        fs::write(&tmp,&encoded)?;
        // A rename ensures that a crash cannot truncate the previous layout.
        // Windows does not replace an existing file with std::fs::rename.
        if cfg!(windows) && path.exists() {fs::remove_file(&path)?;}
        if let Err(error)=fs::rename(&tmp,&path) {
            let _=fs::remove_file(&tmp);
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
        let dock=default_workspace();
        for pane in Pane::ALL {assert!(dock.find_tab(&pane).is_some(),"{}",pane.label());}
    }
    #[test]
    fn workspace_json_roundtrips() {
        let dock=default_workspace();
        let bytes=serde_json::to_vec(&dock).unwrap();
        let decoded:DockState<Pane>=serde_json::from_slice(&bytes).unwrap();
        for pane in Pane::ALL {assert_eq!(dock.find_tab(&pane).is_some(), decoded.find_tab(&pane).is_some());}
    }
}

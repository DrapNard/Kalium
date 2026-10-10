//! Human-readable, versioned native brush presets. No proprietary preset data.
use crate::paint::Brush;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

const MAX_PRESET_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrushPreset {
    format: String,
    version: u32,
    name: String,
    brush: Brush,
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(|c| c.is_control()) {
        bail!("Brush preset name must be 1–128 printable bytes");
    }
    Ok(())
}

pub fn save(brush: &Brush, name: &str, path: &Path) -> Result<()> {
    validate_name(name)?;
    brush.validate()?;
    let content = serde_json::to_vec_pretty(&BrushPreset {
        format: "Kalium Brush".into(),
        version: 1,
        name: name.into(),
        brush: brush.clone(),
    })?;
    fs::write(path, content).with_context(|| format!("Saving preset {}", path.display()))
}

pub fn load(path: &Path) -> Result<(String, Brush)> {
    if fs::metadata(path)?.len() > MAX_PRESET_BYTES {
        bail!("Brush preset exceeds 64 KiB limit");
    }
    let preset: BrushPreset = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("Decoding brush preset {}", path.display()))?;
    if preset.format != "Kalium Brush" || preset.version != 1 {
        bail!("Unsupported Kalium brush preset format/version");
    }
    validate_name(&preset.name)?;
    preset.brush.validate()?;
    Ok((preset.name, preset.brush))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn brush_roundtrip() {
        let path = std::env::temp_dir().join(format!("kalium-{}-preset.kbrush", std::process::id()));
        let brush = Brush::default();
        save(&brush, "Soft Pink", &path).unwrap();
        let (name, recovered) = load(&path).unwrap();
        fs::remove_file(path).ok();
        assert_eq!(name, "Soft Pink");
        assert_eq!(recovered.radius, brush.radius);
        assert_eq!(recovered.color, brush.color);
    }
    #[test]
    fn rejects_bad_values() {
        let mut brush = Brush::default();
        brush.opacity = f32::NAN;
        assert!(brush.validate().is_err());
        brush.opacity = 1.0;
        brush.radius = 0;
        assert!(brush.validate().is_err());
    }
}

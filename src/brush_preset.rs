//! Human-readable, versioned native brush presets. No proprietary preset data.
use crate::paint::{Brush, BrushTip};
use anyhow::{bail, Context, Result};
use image::{ImageReader, Limits};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_PRESET_BYTES: u64 = 64 * 1024;
const MAX_TIP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TIP_DIMENSION: u32 = 64;
const MAX_SHELF_ENTRIES: usize = 512;
const MAX_SHELF_PRESETS: usize = 128;

/// A parsed brush preset found in a user's preset shelf.
#[derive(Debug)]
pub struct ShelfPreset {
    pub path: PathBuf,
    pub name: String,
    pub brush: Brush,
}

/// Results of scanning one directory for native brush presets.
#[derive(Debug, Default)]
pub struct ShelfScan {
    pub presets: Vec<ShelfPreset>,
    pub skipped_files: usize,
}

/// Scan a single directory level for bounded, native `.kbrush` presets.
///
/// Symlinks and non-file entries are ignored. Files with a matching extension
/// that cannot be read or parsed are counted in `skipped_files`.
pub fn scan_shelf(directory: &Path) -> Result<ShelfScan> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ShelfScan::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Reading preset shelf {}", directory.display()));
        }
    };

    let mut candidates = Vec::new();
    for (entry_index, entry) in entries.enumerate() {
        if entry_index >= MAX_SHELF_ENTRIES {
            bail!(
                "Preset shelf {} exceeds the {MAX_SHELF_ENTRIES}-entry limit",
                directory.display()
            );
        }
        let entry = entry
            .with_context(|| format!("Reading an entry in preset shelf {}", directory.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("Inspecting preset shelf entry {}", entry.path().display()))?;
        if file_type.is_file()
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("kbrush"))
        {
            candidates.push(entry.path());
        }
    }

    candidates.sort();
    if candidates.len() > MAX_SHELF_PRESETS {
        bail!(
            "Preset shelf {} exceeds the {MAX_SHELF_PRESETS}-preset limit",
            directory.display()
        );
    }

    let mut scan = ShelfScan::default();
    for path in candidates {
        match load(&path) {
            Ok((name, brush)) => scan.presets.push(ShelfPreset { path, name, brush }),
            Err(_) => scan.skipped_files += 1,
        }
    }
    Ok(scan)
}

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
    let content = encode(brush, name)?;
    fs::write(path, content).with_context(|| format!("Saving preset {}", path.display()))
}

/// Save a preset only if the target path does not already exist.
pub fn save_new(brush: &Brush, name: &str, path: &Path) -> Result<()> {
    let content = encode(brush, name)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Creating preset {}", path.display()))?;
    if let Err(error) = file.write_all(&content) {
        return Err(error).with_context(|| format!("Writing preset {}", path.display()));
    }
    Ok(())
}

fn encode(brush: &Brush, name: &str) -> Result<Vec<u8>> {
    validate_name(name)?;
    brush.validate()?;
    Ok(serde_json::to_vec_pretty(&BrushPreset {
        format: "Kalium Brush".into(),
        version: 2,
        name: name.into(),
        brush: brush.clone(),
    })?)
}

pub fn load(path: &Path) -> Result<(String, Brush)> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_PRESET_BYTES + 1)
        .read_to_end(&mut bytes)?;
    load_bytes(&bytes).with_context(|| format!("Decoding brush preset {}", path.display()))
}

/// Parse a bounded native brush preset from embedded or caller-owned bytes.
pub fn load_bytes(bytes: &[u8]) -> Result<(String, Brush)> {
    if bytes.len() as u64 > MAX_PRESET_BYTES {
        bail!("Brush preset exceeds 64 KiB limit");
    }
    let preset: BrushPreset = serde_json::from_slice(bytes).context("Decoding brush preset")?;
    if preset.format != "Kalium Brush" || !matches!(preset.version, 1 | 2) {
        bail!("Unsupported Kalium brush preset format/version");
    }
    validate_name(&preset.name)?;
    preset.brush.validate()?;
    Ok((preset.name, preset.brush))
}

pub fn load_alpha_tip(path: &Path) -> Result<BrushTip> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Reading alpha brush tip metadata {}", path.display()))?;
    if metadata.len() > MAX_TIP_FILE_BYTES {
        bail!("Alpha brush tip image exceeds 4 MiB");
    }
    let mut reader = ImageReader::open(path)
        .with_context(|| format!("Opening alpha brush tip {}", path.display()))?
        .with_guessed_format()
        .context("Identifying alpha brush tip image format")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_TIP_DIMENSION);
    limits.max_image_height = Some(MAX_TIP_DIMENSION);
    limits.max_alloc = Some((MAX_TIP_DIMENSION * MAX_TIP_DIMENSION * 8) as u64);
    reader.limits(limits);
    let image = reader.decode().context("Decoding alpha brush tip image")?;
    let image = image.to_luma_alpha8();
    let mask = image
        .pixels()
        .map(|pixel| ((pixel.0[0] as u16 * pixel.0[1] as u16 + 127) / 255) as u8)
        .collect();
    BrushTip::alpha(image.width(), image.height(), mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kalium-{}-{label}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_test_preset(path: &Path, name: &str) {
        save(&Brush::default(), name, path).unwrap();
    }

    #[test]
    fn brush_roundtrip() {
        let path =
            std::env::temp_dir().join(format!("kalium-{}-preset.kbrush", std::process::id()));
        let brush = Brush {
            tip: BrushTip::alpha(2, 2, vec![0, 32, 128, 255]).unwrap(),
            ..Brush::default()
        };
        save(&brush, "Soft Pink", &path).unwrap();
        let (name, recovered) = load(&path).unwrap();
        fs::remove_file(path).ok();
        assert_eq!(name, "Soft Pink");
        assert_eq!(recovered.radius, brush.radius);
        assert_eq!(recovered.color, brush.color);
        assert_eq!(recovered.tip, brush.tip);
    }

    #[test]
    fn legacy_v1_preset_without_tip_loads_as_round() {
        let path = std::env::temp_dir().join(format!(
            "kalium-{}-legacy-preset.kbrush",
            std::process::id()
        ));
        let mut brush = serde_json::to_value(Brush::default()).unwrap();
        brush.as_object_mut().unwrap().remove("tip");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "format": "Kalium Brush",
                "version": 1,
                "name": "Legacy",
                "brush": brush,
            }))
            .unwrap(),
        )
        .unwrap();
        let (name, brush) = load(&path).unwrap();
        fs::remove_file(path).ok();
        assert_eq!(name, "Legacy");
        assert!(matches!(brush.tip, BrushTip::Round));
    }

    #[test]
    fn alpha_tip_import_converts_luminance_and_alpha_with_dimension_limit() {
        let path =
            std::env::temp_dir().join(format!("kalium-{}-alpha-tip.png", std::process::id()));
        let mut image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 255, 255, 255]));
        image.put_pixel(0, 0, image::Rgba([128, 128, 128, 128]));
        image.save(&path).unwrap();
        let tip = load_alpha_tip(&path).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(tip, BrushTip::alpha(2, 1, vec![64, 255]).unwrap());

        let oversized_path = std::env::temp_dir().join(format!(
            "kalium-{}-oversized-alpha-tip.png",
            std::process::id()
        ));
        image::RgbaImage::new(65, 1).save(&oversized_path).unwrap();
        assert!(load_alpha_tip(&oversized_path).is_err());
        fs::remove_file(oversized_path).ok();
    }
    #[test]
    fn rejects_bad_values() {
        let brush = Brush {
            opacity: f32::NAN,
            ..Brush::default()
        };
        assert!(brush.validate().is_err());
        let brush = Brush {
            radius: 0,
            ..Brush::default()
        };
        assert!(brush.validate().is_err());
    }

    #[test]
    fn embedded_preset_parser_enforces_byte_limit() {
        assert!(load_bytes(&vec![b' '; (MAX_PRESET_BYTES + 1) as usize]).is_err());
    }

    #[test]
    fn shelf_scan_sorts_candidates_and_matches_extension_case_insensitively() {
        let directory = TestDir::new("shelf-order");
        write_test_preset(&directory.path().join("zeta.kbrush"), "Zeta");
        write_test_preset(&directory.path().join("Alpha.KBRUSH"), "Alpha");
        fs::write(directory.path().join("ignored.txt"), b"not a preset").unwrap();

        let scan = scan_shelf(directory.path()).unwrap();
        assert_eq!(scan.skipped_files, 0);
        assert_eq!(
            scan.presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Zeta"]
        );
        assert_eq!(scan.presets[0].path.file_name().unwrap(), "Alpha.KBRUSH");
    }

    #[test]
    fn shelf_scan_skips_invalid_candidate_files() {
        let directory = TestDir::new("shelf-invalid");
        write_test_preset(&directory.path().join("valid.kbrush"), "Valid");
        fs::write(directory.path().join("invalid.kbrush"), b"not json").unwrap();

        let scan = scan_shelf(directory.path()).unwrap();
        assert_eq!(scan.skipped_files, 1);
        assert_eq!(scan.presets.len(), 1);
        assert_eq!(scan.presets[0].name, "Valid");
    }

    #[test]
    fn shelf_scan_is_nonrecursive_and_missing_directory_is_empty() {
        let directory = TestDir::new("shelf-nonrecursive");
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        write_test_preset(&nested.join("nested.kbrush"), "Nested");

        let scan = scan_shelf(directory.path()).unwrap();
        assert!(scan.presets.is_empty());
        assert_eq!(scan.skipped_files, 0);

        let absent = directory.path().join("absent");
        assert!(scan_shelf(&absent).unwrap().presets.is_empty());
    }

    #[test]
    fn shelf_scan_rejects_more_than_entry_limit() {
        let directory = TestDir::new("shelf-entry-cap");
        for index in 0..=MAX_SHELF_ENTRIES {
            fs::create_dir(directory.path().join(format!("entry-{index:03}"))).unwrap();
        }
        let error = scan_shelf(directory.path()).unwrap_err().to_string();
        assert!(error.contains("512-entry limit"), "{error}");
    }

    #[test]
    fn shelf_scan_rejects_more_than_candidate_limit() {
        let directory = TestDir::new("shelf-preset-cap");
        for index in 0..=MAX_SHELF_PRESETS {
            write_test_preset(
                &directory.path().join(format!("preset-{index:03}.kbrush")),
                "Preset",
            );
        }
        let error = scan_shelf(directory.path()).unwrap_err().to_string();
        assert!(error.contains("128-preset limit"), "{error}");
    }
}

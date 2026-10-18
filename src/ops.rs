//! Non-brush editing operations. These all operate on the native document.
use crate::{
    document::{Channel, Document, MAX_DECODED_BYTES},
    history::History,
    paint::{Brush, BrushMode},
    uv::UvCoverage,
};
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, ImageReader, Rgba};
use std::{collections::VecDeque, path::Path};

/// Fill or erase a single channel, or fill a layer's mask using the chosen mode.
/// Returns false if no pixels changed.
pub fn fill_selected(doc: &mut Document, channel: Channel, brush: &Brush) -> bool {
    let size = doc.size;
    let layer = doc.selected_layer_mut();
    match brush.mode {
        BrushMode::Sample | BrushMode::Bucket => false,
        BrushMode::MaskWhite | BrushMode::MaskBlack => {
            let value = if brush.mode == BrushMode::MaskWhite {
                255
            } else {
                0
            };
            let dirty = layer.mask.pixels().any(|p| p.0[0] != value);
            if dirty {
                for p in layer.mask.pixels_mut() {
                    p.0[0] = value;
                }
            }
            dirty
        }
        BrushMode::Paint | BrushMode::Erase => {
            let color = if brush.mode == BrushMode::Erase {
                [0, 0, 0, 0]
            } else if channel.is_color() {
                brush.color
            } else {
                [brush.grayscale, brush.grayscale, brush.grayscale, 255]
            };
            let image = layer.pixels_mut(channel, size);
            let dirty = image.pixels().any(|p| p.0 != color);
            if dirty {
                for p in image.pixels_mut() {
                    *p = Rgba(color);
                }
            }
            dirty
        }
    }
}

/// 4-connected, non-generative flood fill of the selected layer's active
/// channel. Compares the original pixel values against the seed, bounded by a
/// one-byte visited mask and the document's maximum texture dimensions.
/// Returns true only when a pixel actually changes. The caller owns Undo/Redo.
pub fn flood_fill(
    doc: &mut Document,
    channel: Channel,
    uv: [f32; 2],
    brush: &Brush,
    coverage: Option<&UvCoverage>,
) -> Result<bool> {
    if uv
        .iter()
        .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
    {
        bail!("Flood-fill UV coordinates must be inside [0,1]");
    }
    let size = doc.size;
    if coverage.is_some_and(|c| c.size != size) {
        bail!("UV coverage dimension mismatch");
    }
    let x = (uv[0] * (size - 1) as f32).round() as u32;
    let y = (uv[1] * (size - 1) as f32).round() as u32;
    if coverage.is_some_and(|c| !c.contains(x, y)) {
        return Ok(false);
    }
    let target = if channel.is_color() {
        brush.color
    } else {
        [brush.grayscale, brush.grayscale, brush.grayscale, 255]
    };
    let tolerance = brush.tolerance;
    let layer = doc.selected_layer_mut();
    let texture = layer.pixels_mut(channel, size);
    let seed = texture.get_pixel(x, y).0;
    if seed == target {
        return Ok(false);
    }
    let pixel_count = (size as usize) * (size as usize);
    let mut visited = vec![false; pixel_count];
    let mut todo = VecDeque::new();
    let seed_index = y * size + x;
    visited[seed_index as usize] = true;
    todo.push_back(seed_index);
    let mut changed = false;
    while let Some(index) = todo.pop_front() {
        let px = index % size;
        let py = index / size;
        if coverage.is_some_and(|c| !c.contains(px, py)) {
            continue;
        }
        let current = texture.get_pixel(px, py).0;
        if current
            .iter()
            .zip(seed.iter())
            .any(|(a, b)| a.abs_diff(*b) > tolerance)
        {
            continue;
        }
        texture.put_pixel(px, py, Rgba(target));
        changed = true;
        for neighbor in [
            (px > 0).then_some(index.saturating_sub(1)),
            (px + 1 < size).then_some(index + 1),
            (py > 0).then_some(index.saturating_sub(size)),
            (py + 1 < size).then_some(index + size),
        ]
        .into_iter()
        .flatten()
        {
            let n = neighbor as usize;
            if !visited[n] {
                visited[n] = true;
                todo.push_back(neighbor);
            }
        }
    }
    Ok(changed)
}

/// Import an image as a new editable layer on the active PBR channel.
/// Resampling is explicit; the imported source file is never modified.
pub fn import_texture(doc: &mut Document, channel: Channel, path: &Path) -> Result<()> {
    let (width, height) = image::image_dimensions(path)
        .with_context(|| format!("Reading texture dimensions of {}", path.display()))?;
    validate_import_dimensions(width, height)?;
    let src = ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?
        .to_rgba8();
    let rgba = if src.dimensions() == (doc.size, doc.size) {
        src
    } else {
        image::imageops::resize(&src, doc.size, doc.size, FilterType::Lanczos3)
    };
    doc.add_layer()?;
    let layer = doc.selected_layer_mut();
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("Imported texture");
    layer.name = name.chars().take(128).collect();
    layer.pixels.insert(channel, rgba);
    Ok(())
}

/// Import an image layer as one undoable stack command while retaining any
/// earlier undo history. The imported pixel payload is bounded by the normal
/// texture dimension and history limits.
pub fn import_texture_with_history(
    doc: &mut Document,
    history: &mut History,
    channel: Channel,
    path: &Path,
) -> Result<()> {
    let selected_before = doc.layers[doc.selected].id;
    let next_layer_id_before = doc.next_layer_id;
    import_texture(doc, channel, path)?;
    let imported_id = doc.layers[doc.selected].id;
    if !history.record_added_layer_with_payload(doc, imported_id, selected_before) {
        doc.remove_layer();
        doc.next_layer_id = next_layer_id_before;
        bail!("Imported layer exceeds the 96-MiB undo payload limit");
    }
    Ok(())
}

fn validate_import_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("Input image dimensions must be non-zero");
    }
    let decoded_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .context("Input image decoded RGBA size overflows")?;
    if decoded_bytes > MAX_DECODED_BYTES {
        bail!("Input image decoded RGBA size exceeds the 512 MiB limit");
    }
    if width > 8192 || height > 8192 {
        bail!("Input image dimensions must be in 1..=8192");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Snapshot;
    #[test]
    fn channel_fill_and_erase() {
        let mut doc = Document::new(64);
        let mut brush = Brush {
            color: [32, 64, 128, 255],
            ..Brush::default()
        };
        assert!(fill_selected(&mut doc, Channel::BaseColor, &brush));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(0, 0).0,
            [32, 64, 128, 255]
        );
        assert!(!fill_selected(&mut doc, Channel::BaseColor, &brush));
        brush.mode = BrushMode::Erase;
        assert!(fill_selected(&mut doc, Channel::BaseColor, &brush));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(0, 0).0,
            Channel::BaseColor.background()
        );
    }
    #[test]
    fn fills_mask() {
        let mut doc = Document::new(64);
        let brush = Brush {
            mode: BrushMode::MaskBlack,
            ..Brush::default()
        };
        assert!(fill_selected(&mut doc, Channel::BaseColor, &brush));
        assert_eq!(doc.selected_layer().mask.get_pixel(5, 6).0, [0]);
    }
    #[test]
    fn texture_import_creates_layer() {
        let mut doc = Document::new(64);
        let path = std::env::temp_dir().join(format!("kalium-{}-import.png", std::process::id()));
        image::RgbaImage::from_pixel(8, 8, Rgba([11, 22, 33, 255]))
            .save(&path)
            .unwrap();
        import_texture(&mut doc, Channel::BaseColor, &path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(doc.layers.len(), 2);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(0, 0).0,
            [11, 22, 33, 255]
        );
    }

    #[test]
    fn imported_texture_is_undoable_redoable_and_preserves_prior_history() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, Rgba([80, 90, 100, 255]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            before,
            after,
        });

        let path =
            std::env::temp_dir().join(format!("kalium-{}-undoable-import.png", std::process::id()));
        let expected_name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        image::RgbaImage::from_pixel(2, 2, Rgba([17, 31, 47, 255]))
            .save(&path)
            .unwrap();
        import_texture_with_history(&mut doc, &mut history, Channel::Roughness, &path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(doc.layers.len(), 2);
        assert_eq!(doc.selected_layer().name, expected_name);
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layers.len(), 1);
        assert_eq!(doc.selected_layer().id, 1);
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.layers[0].pixels[&Channel::BaseColor].get_pixel(3, 4).0,
            [0, 0, 0, 0]
        );

        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.layers[0].pixels[&Channel::BaseColor].get_pixel(3, 4).0,
            [80, 90, 100, 255]
        );
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layers.len(), 2);
        assert_eq!(doc.selected_layer().name, expected_name);
        assert_eq!(
            doc.selected_layer().pixels[&Channel::Roughness]
                .get_pixel(32, 32)
                .0,
            [17, 31, 47, 255]
        );
    }

    #[test]
    fn texture_import_rejects_oversized_dimensions_without_adding_layer() {
        let path = declared_png_dimensions("oversized", 8193, 1);
        let mut doc = Document::new(64);
        let layer_count = doc.layers.len();
        let err = import_texture(&mut doc, Channel::BaseColor, &path).unwrap_err();
        std::fs::remove_file(path).ok();
        assert!(err.to_string().contains("1..=8192"));
        assert_eq!(doc.layers.len(), layer_count);
    }

    #[test]
    fn texture_import_rejects_over_budget_dimensions_before_decode() {
        let path = declared_png_dimensions("over-budget", 12_000, 12_000);
        let mut doc = Document::new(64);
        let layer_count = doc.layers.len();
        let err = import_texture(&mut doc, Channel::BaseColor, &path).unwrap_err();
        std::fs::remove_file(path).ok();
        assert!(err.to_string().contains("512 MiB"));
        assert_eq!(doc.layers.len(), layer_count);
    }

    #[test]
    fn texture_import_size_arithmetic_rejects_overflow() {
        let err = validate_import_dimensions(u32::MAX, u32::MAX).unwrap_err();
        assert!(err.to_string().contains("overflows"));
    }

    fn declared_png_dimensions(label: &str, width: u32, height: u32) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kalium-{}-{}-{}.png",
            std::process::id(),
            label,
            width
        ));
        // A tiny valid PNG is enough: only its IHDR is patched, so dimension
        // inspection can exercise hostile declarations without a large image.
        image::RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = png_crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn png_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}

#[cfg(test)]
mod flood_tests {
    use super::*;
    use crate::mesh::{Mesh, Vertex};
    #[test]
    fn fill_is_connected_and_respects_tolerance() {
        let mut d = Document::new(64);
        let img = d.selected_layer_mut().pixels_mut(Channel::BaseColor, 64);
        for y in 0..64 {
            img.put_pixel(31, y, Rgba([255, 0, 0, 255]));
        }
        let b = Brush {
            color: [0, 80, 120, 255],
            ..Brush::default()
        };
        assert!(flood_fill(&mut d, Channel::BaseColor, [0.1, 0.1], &b, None).unwrap());
        let px = &d.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(px.get_pixel(2, 2).0, [0, 80, 120, 255]);
        assert_eq!(px.get_pixel(60, 2).0, [0, 0, 0, 0]);
        assert_eq!(px.get_pixel(31, 2).0, [255, 0, 0, 255]);
    }
    #[test]
    fn fill_respects_coverage() {
        let mesh = Mesh {
            name: "tri".into(),
            vertices: vec![
                Vertex {
                    position: [0., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.1, 0.1],
                    extensions: Default::default(),
                },
                Vertex {
                    position: [1., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.7, 0.1],
                    extensions: Default::default(),
                },
                Vertex {
                    position: [0., 1., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.1, 0.7],
                    extensions: Default::default(),
                },
            ],
            indices: vec![0, 1, 2],
            materials: vec![],
            triangle_materials: vec![],
            extensions: Default::default(),
        };
        let uv = UvCoverage::generate(&mesh, 64).unwrap();
        let mut d = Document::new(64);
        let b = Brush::default();
        assert!(!flood_fill(&mut d, Channel::BaseColor, [0.9, 0.9], &b, Some(&uv)).unwrap());
        assert!(flood_fill(&mut d, Channel::BaseColor, [0.2, 0.2], &b, Some(&uv)).unwrap());
        let px = &d.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(px.get_pixel(60, 60).0[3], 0);
        assert_eq!(px.get_pixel(12, 12).0[3], 255);
    }
}

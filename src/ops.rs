//! Non-brush editing operations. These all operate on the native document.
use crate::{
    document::{Channel, Document, MAX_DECODED_BYTES},
    history::History,
    paint::{Brush, BrushMode},
    uv::UvCoverage,
};
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, GrayImage, ImageReader, Rgba, RgbaImage};
use std::{collections::VecDeque, path::Path};

/// Fill or erase a single channel, or fill a layer's mask using the chosen mode.
/// Returns false if no pixels changed.
#[cfg(test)]
pub fn fill_selected(doc: &mut Document, channel: Channel, brush: &Brush) -> bool {
    let size = doc.size;
    let Some(layer) = doc.selected_paint_layer_mut() else {
        return false;
    };
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
            if !layer.pixels.contains_key(&channel) && color == [0, 0, 0, 0] {
                return false;
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillResult {
    pub changed: bool,
    pub undoable: bool,
}

/// Apply a full-image uniform edit with a swap-based history entry. Existing
/// image storage moves into history instead of being cloned into before/after
/// snapshots and chunk patches.
pub fn fill_selected_with_history(
    doc: &mut Document,
    history: &mut History,
    channel: Channel,
    brush: &Brush,
) -> Result<FillResult> {
    let size = doc.size;
    if doc.selected_paint_layer().is_none() {
        bail!("Selected layer tree node does not accept paint edits");
    }
    match brush.mode {
        BrushMode::Sample | BrushMode::Bucket => Ok(FillResult {
            changed: false,
            undoable: true,
        }),
        BrushMode::MaskWhite | BrushMode::MaskBlack => {
            let value = if brush.mode == BrushMode::MaskWhite {
                255
            } else {
                0
            };
            if doc
                .selected_paint_layer()
                .expect("selected Paint layer was checked")
                .mask
                .as_raw()
                .iter()
                .all(|pixel| *pixel == value)
            {
                return Ok(FillResult {
                    changed: false,
                    undoable: true,
                });
            }
            let selected = doc.selected_paint_layer().expect("checked Paint selection");
            let layer_id = selected.id;
            let previous_capacity = selected.mask.as_raw().capacity();
            let undoable = history.prepare_uniform_edit(previous_capacity);
            if undoable {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size as usize * size as usize)
                    .context("Could not reserve memory for the layer-mask fill")?;
                bytes.resize(size as usize * size as usize, value);
                let replacement = GrayImage::from_raw(size, size, bytes)
                    .context("Could not construct the filled layer mask")?;
                let previous = std::mem::replace(
                    &mut doc
                        .selected_paint_layer_mut()
                        .expect("checked Paint selection")
                        .mask,
                    replacement,
                );
                history.record_uniform_mask(layer_id, value, previous);
            } else {
                for pixel in doc
                    .selected_paint_layer_mut()
                    .expect("checked Paint selection")
                    .mask
                    .as_mut()
                {
                    *pixel = value;
                }
                history.clear();
            }
            Ok(FillResult {
                changed: true,
                undoable,
            })
        }
        BrushMode::Paint | BrushMode::Erase => {
            let value = if brush.mode == BrushMode::Erase {
                [0, 0, 0, 0]
            } else if channel.is_color() {
                brush.color
            } else {
                [brush.grayscale, brush.grayscale, brush.grayscale, 255]
            };
            let (previous_present, previous_capacity, unchanged) = {
                let previous = doc
                    .selected_paint_layer()
                    .expect("checked Paint selection")
                    .pixels
                    .get(&channel);
                if let Some(image) = previous {
                    if image.dimensions() != (size, size) {
                        bail!("Selected channel dimensions do not match the document");
                    }
                    (
                        true,
                        image.as_raw().capacity(),
                        image.pixels().all(|pixel| pixel.0 == value),
                    )
                } else {
                    (false, 0, false)
                }
            };
            if !previous_present && value == [0, 0, 0, 0] {
                return Ok(FillResult {
                    changed: false,
                    undoable: true,
                });
            }
            if unchanged {
                return Ok(FillResult {
                    changed: false,
                    undoable: true,
                });
            }
            if !previous_present {
                let projected = doc
                    .decoded_bytes()
                    .checked_add((size as u64) * (size as u64) * 4)
                    .context("Filled channel size overflows")?;
                if projected > MAX_DECODED_BYTES {
                    bail!("Filling this channel would exceed the 512-MiB decoded texture budget");
                }
            }

            let layer_id = doc
                .selected_paint_layer()
                .expect("checked Paint selection")
                .id;
            let undoable = history.prepare_uniform_edit(previous_capacity);
            if !previous_present {
                doc.selected_paint_layer_mut()
                    .expect("checked Paint selection")
                    .pixels
                    .try_reserve(1)
                    .context("Could not reserve a channel map entry")?;
            }
            if undoable || !previous_present {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size as usize * size as usize * 4)
                    .context("Could not reserve memory for the channel fill")?;
                for _ in 0..size as usize * size as usize {
                    bytes.extend_from_slice(&value);
                }
                let replacement = RgbaImage::from_raw(size, size, bytes)
                    .context("Could not construct the filled channel")?;
                let previous = doc
                    .selected_paint_layer_mut()
                    .expect("checked Paint selection")
                    .pixels
                    .insert(channel, replacement);
                if undoable {
                    history.record_uniform_channel(layer_id, channel, value, previous);
                } else {
                    history.clear();
                }
            } else {
                let image = doc
                    .selected_paint_layer_mut()
                    .expect("checked Paint selection")
                    .pixels
                    .get_mut(&channel)
                    .unwrap();
                for pixel in image.pixels_mut() {
                    *pixel = Rgba(value);
                }
                history.clear();
            }
            Ok(FillResult {
                changed: true,
                undoable,
            })
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
    if doc.selected_paint_layer().is_none() {
        bail!("Selected layer tree node does not accept paint edits");
    }
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
    if !doc
        .selected_paint_layer()
        .expect("checked Paint selection")
        .pixels
        .contains_key(&channel)
        && target == [0, 0, 0, 0]
    {
        return Ok(false);
    }
    let layer = doc
        .selected_paint_layer_mut()
        .expect("checked Paint selection");
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
    import_texture_with_decoded_bytes(doc, channel, path, doc.decoded_bytes())
}

fn import_texture_with_decoded_bytes(
    doc: &mut Document,
    channel: Channel,
    path: &Path,
    existing_decoded_bytes: u64,
) -> Result<()> {
    let (width, height) = image::image_dimensions(path)
        .with_context(|| format!("Reading texture dimensions of {}", path.display()))?;
    validate_import_dimensions(width, height)?;
    validate_import_budget(existing_decoded_bytes, doc.size)?;
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
    let layer = doc
        .selected_paint_layer_mut()
        .expect("new layer must be a Paint node");
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
    let selected_before = doc.layer_tree.selected_node_id;
    let next_layer_id_before = doc.layer_tree.next_node_id;
    import_texture(doc, channel, path)?;
    let imported_id = doc.layer_tree.selected_node_id;
    if !history.record_added_layer_with_payload(doc, imported_id, selected_before) {
        doc.remove_layer();
        doc.layer_tree.next_node_id = next_layer_id_before;
        bail!("Imported layer exceeds the 96-MiB undo payload limit");
    }
    Ok(())
}

#[cfg(test)]
fn import_texture_with_history_decoded_bytes(
    doc: &mut Document,
    history: &mut History,
    channel: Channel,
    path: &Path,
    existing_decoded_bytes: u64,
) -> Result<()> {
    let selected_before = doc.layer_tree.selected_node_id;
    let next_layer_id_before = doc.layer_tree.next_node_id;
    import_texture_with_decoded_bytes(doc, channel, path, existing_decoded_bytes)?;
    let imported_id = doc.layer_tree.selected_node_id;
    if !history.record_added_layer_with_payload(doc, imported_id, selected_before) {
        doc.remove_layer();
        doc.layer_tree.next_node_id = next_layer_id_before;
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

fn validate_import_budget(existing_decoded_bytes: u64, target_size: u32) -> Result<()> {
    let target_pixels = u64::from(target_size)
        .checked_mul(u64::from(target_size))
        .context("Target texture dimensions overflow")?;
    // A new layer owns an L8 mask plus one imported RGBA channel at the
    // document's working resolution. Check both before decoding or resizing.
    let additional_bytes = target_pixels
        .checked_mul(5)
        .context("Imported layer decoded size overflows")?;
    if existing_decoded_bytes
        .checked_add(additional_bytes)
        .is_none_or(|projected| projected > MAX_DECODED_BYTES)
    {
        bail!("Importing this layer would exceed the 512-MiB decoded texture budget");
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
    fn transparent_fill_and_flood_fill_leave_absent_channel_absent() {
        let mut doc = Document::new(64);
        let mut brush = Brush {
            mode: BrushMode::Erase,
            color: [0, 0, 0, 0],
            ..Brush::default()
        };

        assert!(!fill_selected(&mut doc, Channel::BaseColor, &brush));
        assert!(!doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));

        brush.mode = BrushMode::Paint;
        assert!(!flood_fill(&mut doc, Channel::BaseColor, [0.5, 0.5], &brush, None,).unwrap());
        assert!(!doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
    }

    #[test]
    fn uniform_fill_history_swaps_absent_and_present_channel_buffers() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let brush = Brush {
            color: [17, 33, 91, 255],
            ..Brush::default()
        };
        let result =
            fill_selected_with_history(&mut doc, &mut history, Channel::BaseColor, &brush).unwrap();
        assert_eq!(
            result,
            FillResult {
                changed: true,
                undoable: true
            }
        );
        assert!(history.can_undo());
        assert!(history.undo(&mut doc));
        assert!(!doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert!(doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels[&Channel::BaseColor]
            .pixels()
            .all(|pixel| pixel.0 == brush.color));

        history.clear();
        let previous: Vec<u8> = (0..64 * 64 * 4).map(|byte| (byte % 251) as u8).collect();
        doc.selected_paint_layer_mut()
            .expect("test selects a Paint layer")
            .pixels
            .insert(
                Channel::BaseColor,
                RgbaImage::from_raw(64, 64, previous.clone()).unwrap(),
            );
        let result =
            fill_selected_with_history(&mut doc, &mut history, Channel::BaseColor, &brush).unwrap();
        assert!(result.changed && result.undoable);
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .pixels[&Channel::BaseColor]
                .as_raw(),
            &previous
        );
        assert!(history.redo(&mut doc));
        assert!(doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels[&Channel::BaseColor]
            .pixels()
            .all(|pixel| pixel.0 == brush.color));
    }

    #[test]
    fn uniform_mask_reset_and_invert_history_are_exact_without_image_snapshots() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let mut previous = doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .mask
            .clone();
        for (index, pixel) in previous.as_mut().iter_mut().enumerate() {
            *pixel = (index % 256) as u8;
        }
        doc.selected_paint_layer_mut()
            .expect("test selects a Paint layer")
            .mask = previous.clone();

        let brush = Brush {
            mode: BrushMode::MaskWhite,
            ..Brush::default()
        };
        assert!(
            fill_selected_with_history(&mut doc, &mut history, Channel::BaseColor, &brush,)
                .unwrap()
                .changed
        );
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .mask,
            previous
        );
        assert!(history.redo(&mut doc));
        assert!(doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .mask
            .as_raw()
            .iter()
            .all(|value| *value == 255));

        let before_invert = doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .mask
            .clone();
        doc.invert_selected_mask();
        let after_invert = doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .mask
            .clone();
        history.record_mask_invert(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .id,
        );
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .mask,
            before_invert
        );
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .mask,
            after_invert
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
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .mask
                .get_pixel(5, 6)
                .0,
            [0]
        );
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
        assert_eq!(doc.layer_tree.roots.len(), 2);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(0, 0).0,
            [11, 22, 33, 255]
        );
    }

    #[test]
    fn imported_texture_is_undoable_redoable_and_preserves_prior_history() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_paint_layer_mut()
            .expect("test selects a Paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, Rgba([80, 90, 100, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
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
        assert_eq!(doc.layer_tree.roots.len(), 2);
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .name,
            expected_name
        );
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .id,
            1
        );
        assert!(history.undo(&mut doc));
        assert!(!doc
            .layer_tree
            .paint_layer(1)
            .expect("root Paint layer")
            .pixels
            .contains_key(&Channel::BaseColor));

        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.layer_tree
                .paint_layer(1)
                .expect("root Paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(3, 4)
                .0,
            [80, 90, 100, 255]
        );
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layer_tree.roots.len(), 2);
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .name,
            expected_name
        );
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .pixels[&Channel::Roughness]
                .get_pixel(32, 32)
                .0,
            [17, 31, 47, 255]
        );
    }

    #[test]
    fn texture_import_rejects_oversized_dimensions_without_adding_layer() {
        let path = declared_png_dimensions("oversized", 8193, 1);
        let mut doc = Document::new(64);
        let layer_count = doc.layer_tree.roots.len();
        let err = import_texture(&mut doc, Channel::BaseColor, &path).unwrap_err();
        std::fs::remove_file(path).ok();
        assert!(err.to_string().contains("1..=8192"));
        assert_eq!(doc.layer_tree.roots.len(), layer_count);
    }

    #[test]
    fn texture_import_rejects_over_budget_dimensions_before_decode() {
        let path = declared_png_dimensions("over-budget", 12_000, 12_000);
        let mut doc = Document::new(64);
        let layer_count = doc.layer_tree.roots.len();
        let err = import_texture(&mut doc, Channel::BaseColor, &path).unwrap_err();
        std::fs::remove_file(path).ok();
        assert!(err.to_string().contains("512 MiB"));
        assert_eq!(doc.layer_tree.roots.len(), layer_count);
    }

    #[test]
    fn imported_layer_budget_accepts_exact_limit_and_rejects_one_byte_over() {
        let layer_bytes = (64_u64 * 64) * 5;
        assert!(validate_import_budget(MAX_DECODED_BYTES - layer_bytes, 64).is_ok());
        let error = validate_import_budget(MAX_DECODED_BYTES - layer_bytes + 1, 64).unwrap_err();
        assert!(error.to_string().contains("512-MiB decoded texture budget"));
        assert!(validate_import_budget(u64::MAX, 64).is_err());
    }

    #[test]
    fn over_budget_texture_import_preserves_document_and_redo_history() {
        let path = std::env::temp_dir().join(format!(
            "kalium-{}-over-budget-import-transaction.png",
            std::process::id()
        ));
        image::RgbaImage::from_pixel(1, 1, Rgba([11, 22, 33, 255]))
            .save(&path)
            .unwrap();
        let mut encoded = std::fs::read(&path).unwrap();
        // Keep the PNG header readable for dimensions, but corrupt its image
        // payload so this must fail on budget preflight before pixel decoding.
        encoded[42] ^= 0xff;
        std::fs::write(&path, encoded).unwrap();

        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_paint_layer_mut()
            .expect("test selects a Paint layer")
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, Rgba([80, 90, 100, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        });
        assert!(history.undo(&mut doc));

        let decoded_before = doc.decoded_bytes();
        let layer_count_before = doc.layer_tree.roots.len();
        let selected_before = doc.layer_tree.selected_node_id;
        let next_layer_id_before = doc.layer_tree.next_node_id;
        let error = import_texture_with_history_decoded_bytes(
            &mut doc,
            &mut history,
            Channel::Roughness,
            &path,
            MAX_DECODED_BYTES - (64_u64 * 64 * 5) + 1,
        )
        .unwrap_err();
        std::fs::remove_file(path).ok();

        assert!(error.to_string().contains("512-MiB decoded texture budget"));
        assert_eq!(doc.decoded_bytes(), decoded_before);
        assert_eq!(doc.layer_tree.roots.len(), layer_count_before);
        assert_eq!(doc.layer_tree.selected_node_id, selected_before);
        assert_eq!(doc.layer_tree.next_node_id, next_layer_id_before);
        assert!(!doc
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels
            .contains_key(&Channel::Roughness));

        // The preexisting redo branch remains usable after the rejected import.
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_paint_layer()
                .expect("test selects a Paint layer")
                .pixels[&Channel::BaseColor]
                .get_pixel(3, 4)
                .0,
            [80, 90, 100, 255]
        );
        assert_eq!(doc.layer_tree.roots.len(), layer_count_before);
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
        let img = d
            .selected_paint_layer_mut()
            .expect("test selects a Paint layer")
            .pixels_mut(Channel::BaseColor, 64);
        for y in 0..64 {
            img.put_pixel(31, y, Rgba([255, 0, 0, 255]));
        }
        let b = Brush {
            color: [0, 80, 120, 255],
            ..Brush::default()
        };
        assert!(flood_fill(&mut d, Channel::BaseColor, [0.1, 0.1], &b, None).unwrap());
        let px = &d
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels[&Channel::BaseColor];
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
                    tangent: None,
                    extensions: Default::default(),
                },
                Vertex {
                    position: [1., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.7, 0.1],
                    tangent: None,
                    extensions: Default::default(),
                },
                Vertex {
                    position: [0., 1., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.1, 0.7],
                    tangent: None,
                    extensions: Default::default(),
                },
            ],
            indices: vec![0, 1, 2],
            materials: vec![],
            triangle_materials: vec![],
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        };
        let uv = UvCoverage::generate(&mesh, 64).unwrap();
        let mut d = Document::new(64);
        let b = Brush::default();
        assert!(!flood_fill(&mut d, Channel::BaseColor, [0.9, 0.9], &b, Some(&uv)).unwrap());
        assert!(flood_fill(&mut d, Channel::BaseColor, [0.2, 0.2], &b, Some(&uv)).unwrap());
        let px = &d
            .selected_paint_layer()
            .expect("test selects a Paint layer")
            .pixels[&Channel::BaseColor];
        assert_eq!(px.get_pixel(60, 60).0[3], 0);
        assert_eq!(px.get_pixel(12, 12).0[3], 255);
    }
}

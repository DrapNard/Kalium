//! Non-brush editing operations. These all operate on the native document.
use crate::{document::{Channel, Document}, paint::{Brush, BrushMode}};
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, ImageReader, Rgba};
use std::path::Path;

/// Fill or erase a single channel, or fill a layer's mask using the chosen mode.
/// Returns false if no pixels changed.
pub fn fill_selected(doc: &mut Document, channel: Channel, brush: &Brush) -> bool {
    let size = doc.size;
    let layer = doc.selected_layer_mut();
    match brush.mode {
        BrushMode::Sample => false,
        BrushMode::MaskWhite | BrushMode::MaskBlack => {
            let value = if brush.mode == BrushMode::MaskWhite { 255 } else { 0 };
            let dirty = layer.mask.pixels().any(|p| p.0[0] != value);
            if dirty { for p in layer.mask.pixels_mut() { p.0[0] = value; } }
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
            if dirty { for p in image.pixels_mut() { *p = Rgba(color); } }
            dirty
        }
    }
}

/// Import an image as a new editable layer on the active PBR channel.
/// Resampling is explicit; the imported source file is never modified.
pub fn import_texture(doc: &mut Document, channel: Channel, path: &Path) -> Result<()> {
    let (width, height) = image::image_dimensions(path)
        .with_context(|| format!("Reading texture dimensions of {}", path.display()))?;
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        bail!("Input image dimensions must be in 1..=8192");
    }
    let src = ImageReader::open(path)?.with_guessed_format()?.decode()?.to_rgba8();
    let rgba = if src.dimensions() == (doc.size, doc.size) {
        src
    } else {
        image::imageops::resize(&src, doc.size, doc.size, FilterType::Lanczos3)
    };
    doc.add_layer()?;
    let layer = doc.selected_layer_mut();
    let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("Imported texture");
    layer.name = name.chars().take(128).collect();
    layer.pixels.insert(channel, rgba);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn channel_fill_and_erase() {
        let mut doc=Document::new(64);
        let mut brush=Brush::default();
        brush.color=[32,64,128,255];
        assert!(fill_selected(&mut doc,Channel::BaseColor,&brush));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(0,0).0,[32,64,128,255]);
        assert!(!fill_selected(&mut doc,Channel::BaseColor,&brush));
        brush.mode=BrushMode::Erase;
        assert!(fill_selected(&mut doc,Channel::BaseColor,&brush));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(0,0).0,Channel::BaseColor.background());
    }
    #[test]
    fn fills_mask() {
        let mut doc=Document::new(64);
        let mut brush=Brush::default();
        brush.mode=BrushMode::MaskBlack;
        assert!(fill_selected(&mut doc,Channel::BaseColor,&brush));
        assert_eq!(doc.selected_layer().mask.get_pixel(5,6).0,[0]);
    }
    #[test]
    fn texture_import_creates_layer() {
        let mut doc=Document::new(64);
        let path=std::env::temp_dir().join(format!("kalium-{}-import.png",std::process::id()));
        image::RgbaImage::from_pixel(8,8,Rgba([11,22,33,255])).save(&path).unwrap();
        import_texture(&mut doc,Channel::BaseColor,&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(doc.layers.len(),2);
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(0,0).0,[11,22,33,255]);
    }
}

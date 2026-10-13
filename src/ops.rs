//! Non-brush editing operations. These all operate on the native document.
use crate::{document::{Channel, Document}, paint::{Brush, BrushMode}, uv::UvCoverage};
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, ImageReader, Rgba};
use std::{path::Path, collections::VecDeque};

/// Fill or erase a single channel, or fill a layer's mask using the chosen mode.
/// Returns false if no pixels changed.
pub fn fill_selected(doc: &mut Document, channel: Channel, brush: &Brush) -> bool {
    let size = doc.size;
    let layer = doc.selected_layer_mut();
    match brush.mode {
        BrushMode::Sample | BrushMode::Bucket => false,
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


/// 4-connected, non-generative flood fill of the selected layer's active
/// channel. Compares the original pixel values against the seed, bounded by a
/// one-byte visited mask and the document's maximum texture dimensions.
/// Returns true only when a pixel actually changes. The caller owns Undo/Redo.
pub fn flood_fill(doc: &mut Document, channel: Channel, uv: [f32;2], brush: &Brush,
                  coverage: Option<&UvCoverage>) -> Result<bool> {
    if uv.iter().any(|v|!v.is_finite() || !(0.0..=1.0).contains(v)) {
        bail!("Flood-fill UV coordinates must be inside [0,1]");
    }
    let size=doc.size;
    if coverage.is_some_and(|c| c.size!=size) {bail!("UV coverage dimension mismatch");}
    let x=(uv[0]*(size-1) as f32).round() as u32;
    let y=(uv[1]*(size-1) as f32).round() as u32;
    if coverage.is_some_and(|c| !c.contains(x,y)) {return Ok(false);}
    let target=if channel.is_color() {brush.color}
        else {[brush.grayscale,brush.grayscale,brush.grayscale,255]};
    let tolerance=brush.tolerance;
    let layer=doc.selected_layer_mut();
    let texture=layer.pixels_mut(channel,size);
    let seed=texture.get_pixel(x,y).0;
    if seed==target {return Ok(false);}
    let pixel_count=(size as usize)*(size as usize);
    let mut visited=vec![false;pixel_count];
    let mut todo=VecDeque::new();
    let seed_index=y*size+x;
    visited[seed_index as usize]=true;
    todo.push_back(seed_index);
    let mut changed=false;
    while let Some(index)=todo.pop_front() {
        let px=index%size;
        let py=index/size;
        if coverage.is_some_and(|c| !c.contains(px,py)) {continue;}
        let current=texture.get_pixel(px,py).0;
        if current.iter().zip(seed.iter()).any(|(a,b)|a.abs_diff(*b)>tolerance) {continue;}
        texture.put_pixel(px,py,Rgba(target));
        changed=true;
        for neighbor in [
            (px>0).then_some(index.saturating_sub(1)),
            (px+1<size).then_some(index+1),
            (py>0).then_some(index.saturating_sub(size)),
            (py+1<size).then_some(index+size),
        ].into_iter().flatten() {
            let n=neighbor as usize;
            if !visited[n] {
                visited[n]=true;
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

#[cfg(test)]
mod flood_tests {
    use super::*;
    use crate::mesh::{Mesh,Vertex};
    #[test]
    fn fill_is_connected_and_respects_tolerance() {
        let mut d=Document::new(64);
        let img=d.selected_layer_mut().pixels_mut(Channel::BaseColor,64);
        for y in 0..64 { img.put_pixel(31,y,Rgba([255,0,0,255])); }
        let b=Brush{color:[0,80,120,255],..Brush::default()};
        assert!(flood_fill(&mut d,Channel::BaseColor,[0.1,0.1],&b,None).unwrap());
        let px=&d.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(px.get_pixel(2,2).0,[0,80,120,255]);
        assert_eq!(px.get_pixel(60,2).0,[0,0,0,0]);
        assert_eq!(px.get_pixel(31,2).0,[255,0,0,255]);
    }
    #[test]
    fn fill_respects_coverage() {
        let mesh=Mesh{name:"tri".into(),vertices:vec![
            Vertex{position:[0.,0.,0.],normal:[0.,0.,1.],uv:[0.1,0.1]},
            Vertex{position:[1.,0.,0.],normal:[0.,0.,1.],uv:[0.7,0.1]},
            Vertex{position:[0.,1.,0.],normal:[0.,0.,1.],uv:[0.1,0.7]},
        ],indices:vec![0,1,2],materials:vec![],triangle_materials:vec![]};
        let uv=UvCoverage::generate(&mesh,64).unwrap();
        let mut d=Document::new(64);
        let b=Brush::default();
        assert!(!flood_fill(&mut d,Channel::BaseColor,[0.9,0.9],&b,Some(&uv)).unwrap());
        assert!(flood_fill(&mut d,Channel::BaseColor,[0.2,0.2],&b,Some(&uv)).unwrap());
        let px=&d.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(px.get_pixel(60,60).0[3],0);
        assert_eq!(px.get_pixel(12,12).0[3],255);
    }
}

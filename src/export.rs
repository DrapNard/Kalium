use crate::document::{Channel,Document};
use anyhow::{Context,Result};
use image::{Rgba,RgbaImage};
use std::path::Path;

pub fn export_textures(doc:&Document,dir:&Path)->Result<usize> {
    std::fs::create_dir_all(dir)?;
    for channel in Channel::ALL {
        let image=doc.composite(channel);
        let path=dir.join(format!("Kalium_{}.png",channel.filename()));
        image.save(&path).with_context(||format!("Writing {}",path.display()))?;
    }
    let mut base=doc.composite(Channel::BaseColor);
    let opacity=doc.composite(Channel::Opacity);
    for (base,opa) in base.pixels_mut().zip(opacity.pixels()) {
        base.0[3]=opa.0[0];
    }
    base.save(dir.join("Kalium_BaseColor_Alpha.png"))?;
    let metal=doc.composite(Channel::Metallic);
    let rough=doc.composite(Channel::Roughness);
    let ao=doc.composite(Channel::Occlusion);
    let mut orm=RgbaImage::new(doc.size,doc.size);
    let mut unity=RgbaImage::new(doc.size,doc.size);
    for y in 0..doc.size {for x in 0..doc.size {
        let occ=ao.get_pixel(x,y).0[0];
        let r=rough.get_pixel(x,y).0[0];
        let m=metal.get_pixel(x,y).0[0];
        orm.put_pixel(x,y,Rgba([occ,r,m,255]));
        unity.put_pixel(x,y,Rgba([m,0,0,255-r]));
    }}
    orm.save(dir.join("Kalium_ORM_Unreal_Godot.png"))?;
    unity.save(dir.join("Kalium_MetallicSmoothness_Unity.png"))?;
    Ok(11)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exported_masks_encode_expected_channels() {
        let doc=Document::new(64);
        let dir=std::env::temp_dir().join(format!("kalium-export-test-{}",std::process::id()));
        let count=export_textures(&doc,&dir).unwrap();
        assert_eq!(count,11);
        let pixel=image::open(dir.join("Kalium_ORM_Unreal_Godot.png")).unwrap().to_rgba8().get_pixel(0,0).0;
        assert_eq!(&pixel[..3],&[255,170,0]);
        std::fs::remove_dir_all(dir).ok();
    }
}

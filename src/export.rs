use crate::document::{Channel,Document};
use anyhow::{anyhow,Context,Result};
use image::{Rgba,RgbaImage};
use std::path::Path;

fn write_maps<F>(size:u32,dir:&Path,mut composite:F)->Result<usize>
where F: FnMut(Channel)->Result<RgbaImage> {
    std::fs::create_dir_all(dir)?;
    for channel in Channel::ALL {
        let image=composite(channel)?;
        let path=dir.join(format!("Kalium_{}.png",channel.filename()));
        image.save(&path).with_context(||format!("Writing {}",path.display()))?;
    }
    let mut base=composite(Channel::BaseColor)?;
    let opacity=composite(Channel::Opacity)?;
    for (base,opa) in base.pixels_mut().zip(opacity.pixels()) {
        base.0[3]=opa.0[0];
    }
    base.save(dir.join("Kalium_BaseColor_Alpha.png"))?;
    let metal=composite(Channel::Metallic)?;
    let rough=composite(Channel::Roughness)?;
    let ao=composite(Channel::Occlusion)?;
    let mut orm=RgbaImage::new(size,size);
    let mut unity=RgbaImage::new(size,size);
    for y in 0..size {for x in 0..size {
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

/// Exports the active texture set (preserves the legacy output naming).
pub fn export_textures(doc:&Document,dir:&Path)->Result<usize> {
    write_maps(doc.size,dir,|channel| Ok(doc.composite(channel)))
}

/// Exports each set into a distinct numbered folder; titles are sanitized for
/// filesystem safety and IDs prevent collisions between identical names.
pub fn export_all_texture_sets(doc:&Document,dir:&Path)->Result<usize> {
    doc.validate()?;
    let mut count=0;
    for (id,name,size,_) in doc.texture_set_list() {
        let safe:String=name.chars().map(|c|if c.is_ascii_alphanumeric() || c=='-' || c=='_' {c} else {'_'}).take(48).collect();
        let directory=dir.join(format!("{id:03}_{safe}"));
        count+=write_maps(size,&directory,|channel| doc.composite_for_set(id,channel)
            .ok_or_else(||anyhow!("Unknown texture set {id}")))?;
    }
    Ok(count)
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
    #[test]
    fn each_texture_set_exports_to_separate_folder() {
        let mut doc=Document::new(64);
        doc.add_texture_set("Armor / Main").unwrap();
        let dir=std::env::temp_dir().join(format!("kalium-export-sets-test-{}",std::process::id()));
        let count=export_all_texture_sets(&doc,&dir).unwrap();
        assert_eq!(count,22);
        assert!(dir.join("001_Default/Kalium_BaseColor.png").exists());
        assert!(dir.join("002_Armor___Main/Kalium_ORM_Unreal_Godot.png").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}

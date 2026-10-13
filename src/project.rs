//! Native .kalium projects are versioned ZIP archives.
//! v4 adds mesh material bindings while preserving v1/v2/v3 import.
//! Unknown ZIP data is rejected rather than silently discarded during a save.
use crate::document::{BlendMode,Channel,Document,Layer,TextureSet,MAX_LAYERS,MAX_SIZE,MAX_TEXTURE_SETS,MAX_DECODED_BYTES};
use crate::mesh::Mesh;
use anyhow::{anyhow,bail,Context,Result};
use serde::{Deserialize,Serialize};
use std::{collections::HashSet,fs::File,io::{Read,Write},path::Path};
use zip::{ZipArchive,ZipWriter,write::FileOptions,CompressionMethod};

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format:String,
    version:u32,
    mesh:Mesh,
    // Legacy root layout is retained as the active set in v3 and v4.
    size:u32,
    selected:usize,
    next_layer_id:u32,
    layers:Vec<LayerManifest>,
    #[serde(default)] active_set_id:u32,
    #[serde(default)] active_set_name:String,
    #[serde(default)] next_set_id:u32,
    #[serde(default)] texture_sets:Vec<SetManifest>,
    #[serde(default)] material_bindings:Vec<Option<u32>>,
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct SetManifest {
    id:u32,
    name:String,
    size:u32,
    selected:usize,
    next_layer_id:u32,
    layers:Vec<LayerManifest>,
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerManifest {
    id:u32,
    name:String,
    visible:bool,
    opacity:f32,
    blend:BlendMode,
    channels:Vec<Channel>,
    #[serde(default)] disabled_channels:Vec<Channel>,
}

fn layer_manifests(layers:&[Layer])->Vec<LayerManifest> {
    layers.iter().map(|l|LayerManifest {
        id:l.id,name:l.name.clone(),visible:l.visible,opacity:l.opacity,blend:l.blend,
        channels:Channel::ALL.into_iter().filter(|c|l.pixels.contains_key(c)).collect(),
        disabled_channels:Channel::ALL.into_iter().filter(|c|l.disabled_channels.contains(c)).collect(),
    }).collect()
}

fn prefix(set_id:Option<u32>)->String {
    match set_id {
        Some(id)=>format!("texture_sets/{id}/layers"),
        None=>"layers".to_owned(),
    }
}

fn image_name(set_id:Option<u32>,layer_id:u32,channel:Option<Channel>)->String {
    let suffix=channel.map_or_else(||"mask.png".to_owned(),|c|format!("{}.png",c.filename()));
    format!("{}/{layer_id}/{suffix}",prefix(set_id))
}

fn write_layers(writer:&mut ZipWriter<File>,opts:FileOptions,set_id:Option<u32>,layers:&[Layer])->Result<()> {
    for layer in layers {
        writer.start_file(image_name(set_id,layer.id,None),opts)?;
        let mut buf=std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(layer.mask.clone())
            .write_to(&mut buf,image::ImageFormat::Png)?;
        writer.write_all(&buf.into_inner())?;
        for (channel,pixels) in &layer.pixels {
            writer.start_file(image_name(set_id,layer.id,Some(*channel)),opts)?;
            let mut buf=std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(pixels.clone())
                .write_to(&mut buf,image::ImageFormat::Png)?;
            writer.write_all(&buf.into_inner())?;
        }
    }
    Ok(())
}

pub fn save(doc:&Document,path:&Path)->Result<()> {
    doc.validate()?;
    let manifest=Manifest {
        format:"Kalium Project".into(),version:4,mesh:doc.mesh.clone(),
        size:doc.size,selected:doc.selected,next_layer_id:doc.next_layer_id,
        layers:layer_manifests(&doc.layers),
        active_set_id:doc.active_set_id,active_set_name:doc.active_set_name.clone(),
        next_set_id:doc.next_set_id,
        material_bindings:doc.material_bindings.clone(),
        texture_sets:doc.texture_sets.iter().map(|set|SetManifest {
            id:set.id,name:set.name.clone(),size:set.size,selected:set.selected,
            next_layer_id:set.next_layer_id,layers:layer_manifests(&set.layers),
        }).collect(),
    };
    let filename=path.file_name().ok_or_else(||anyhow!("Invalid save filename"))?;
    let mut temp_name=filename.to_os_string();
    temp_name.push(".tmp");
    let tmp=path.with_file_name(temp_name);
    let result=(||->Result<()> {
        let file=File::create(&tmp)?;
        let mut writer=ZipWriter::new(file);
        let opts=FileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file("manifest.json",opts)?;
        writer.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        write_layers(&mut writer,opts,None,&doc.layers)?;
        for set in &doc.texture_sets {
            write_layers(&mut writer,opts,Some(set.id),&set.layers)?;
        }
        writer.finish()?;
        Ok(())
    })();
    if let Err(err)=result {let _=std::fs::remove_file(&tmp);return Err(err);}
    std::fs::rename(&tmp,path).with_context(||format!("Replacing project {}",path.display()))?;
    Ok(())
}

fn read_entry(zip:&mut ZipArchive<File>,name:&str,max_bytes:u64)->Result<Vec<u8>> {
    let mut entry=zip.by_name(name).with_context(||format!("Missing {name}"))?;
    if entry.size()>max_bytes {bail!("File '{name}' exceeds safety limit");}
    let mut buf=Vec::with_capacity(entry.size() as usize);
    (&mut entry).take(max_bytes+1).read_to_end(&mut buf)?;
    if buf.len() as u64>max_bytes {bail!("File '{name}' expands beyond safety limit");}
    Ok(buf)
}

fn account_layers(
    valid:&mut HashSet<String>,
    total:&mut u64,
    set_id:Option<u32>,
    size:u32,
    layers:&[LayerManifest],
)->Result<()> {
    if !(64..=MAX_SIZE).contains(&size) {bail!("Invalid texture dimension");}
    if layers.is_empty() || layers.len()>MAX_LAYERS {bail!("Invalid layer count");}
    let mut ids=HashSet::new();
    for layer in layers {
        if !ids.insert(layer.id) {bail!("Duplicate layer id in texture set");}
        valid.insert(image_name(set_id,layer.id,None));
        *total=(*total).saturating_add((size as u64).pow(2));
        let mut channels=HashSet::new();
        for &channel in &layer.channels {
            if !channels.insert(channel) {bail!("Duplicate channel in layer manifest");}
            valid.insert(image_name(set_id,layer.id,Some(channel)));
            *total=(*total).saturating_add((size as u64).pow(2)*4);
        }
    }
    Ok(())
}

fn load_layers(
    zip:&mut ZipArchive<File>,set_id:Option<u32>,size:u32, layers:Vec<LayerManifest>
)->Result<Vec<Layer>> {
    let mut result=Vec::with_capacity(layers.len());
    let rgba_bytes=(size as u64).pow(2)*4;
    for meta in layers {
        let mut layer=Layer::new(meta.id,meta.name,size);
        layer.visible=meta.visible;
        layer.opacity=meta.opacity;
        layer.blend=meta.blend;
        layer.disabled_channels=meta.disabled_channels.into_iter().collect();
        let bytes=read_entry(zip,&image_name(set_id,meta.id,None),rgba_bytes+4*1024*1024)?;
        layer.mask=image::load_from_memory(&bytes)?.to_luma8();
        if layer.mask.dimensions()!=(size,size) {bail!("Mask dimensions do not match project");}
        for channel in meta.channels {
            let bytes=read_entry(zip,&image_name(set_id,meta.id,Some(channel)),rgba_bytes+4*1024*1024)?;
            let rgba=image::load_from_memory(&bytes)?.to_rgba8();
            if rgba.dimensions()!=(size,size) {bail!("Channel dimensions do not match project");}
            layer.pixels.insert(channel,rgba);
        }
        result.push(layer);
    }
    Ok(result)
}

pub fn load(path:&Path)->Result<Document> {
    let file=File::open(path)?;
    let mut zip=ZipArchive::new(file).context("Not a valid Kalium project ZIP")?;
    let limit=1+MAX_TEXTURE_SETS*MAX_LAYERS*(Channel::ALL.len()+2);
    if zip.len()>limit {bail!("Project contains too many ZIP entries");}
    let manifest:Manifest=serde_json::from_slice(&read_entry(&mut zip,"manifest.json",48*1024*1024)?)?;
    if manifest.format!="Kalium Project" || !matches!(manifest.version,1|2|3|4) {
        bail!("Unsupported Kalium project version");
    }
    if manifest.version<4 && !manifest.material_bindings.is_empty() {
        bail!("Legacy project unexpectedly contains material binding metadata");
    }
    if manifest.version==4 && manifest.material_bindings.len()!=manifest.mesh.material_count() {
        bail!("Material binding count does not match mesh");
    }
    if manifest.version<3 && (!manifest.texture_sets.is_empty() || manifest.active_set_id!=0) {
        bail!("Legacy project contains unexpected texture set metadata");
    }
    manifest.mesh.validate()?;
    let mut valid=HashSet::from(["manifest.json".to_owned()]);
    let mut decoded_bytes=0;
    account_layers(&mut valid,&mut decoded_bytes,None,manifest.size,&manifest.layers)?;
    if manifest.texture_sets.len()>=MAX_TEXTURE_SETS {bail!("Too many texture sets");}
    let mut set_ids=HashSet::new();
    if manifest.version>=3 {
        if manifest.active_set_id==0 || manifest.next_set_id<=manifest.active_set_id {
            bail!("Invalid active texture set identifier");
        }
        set_ids.insert(manifest.active_set_id);
        for set in &manifest.texture_sets {
            if set.id==0 || set.id>=manifest.next_set_id || !set_ids.insert(set.id) {
                bail!("Duplicate/invalid texture set identifier");
            }
            account_layers(&mut valid,&mut decoded_bytes,Some(set.id),set.size,&set.layers)?;
        }
    }
    if decoded_bytes>MAX_DECODED_BYTES {bail!("Project exceeds 512-MiB decoded texture budget");}
    let mut seen=HashSet::new();
    for idx in 0..zip.len() {
        let entry=zip.by_index(idx)?;
        if !seen.insert(entry.name().to_owned()) {bail!("Duplicate ZIP entry '{}'; refusing ambiguous input",entry.name());}
        if entry.is_dir() {
            let name=entry.name().trim_end_matches('/');
            let is_parent=valid.iter().any(|path|path.starts_with(&format!("{name}/")));
            if !is_parent {bail!("Unexpected ZIP directory '{}'",entry.name());}
        } else if !valid.contains(entry.name()) {
            bail!("Unknown archive entry '{}'; refusing lossy import",entry.name());
        }
    }
    let mut doc=Document::new(manifest.size);
    doc.mesh=manifest.mesh;
    doc.material_bindings=if manifest.version>=4 {manifest.material_bindings}
        else {vec![None; doc.mesh.material_count()]};
    doc.size=manifest.size;
    doc.selected=manifest.selected;
    doc.next_layer_id=manifest.next_layer_id;
    doc.layers=load_layers(&mut zip,None,manifest.size,manifest.layers)?;
    if manifest.version>=3 {
        doc.active_set_id=manifest.active_set_id;
        doc.active_set_name=manifest.active_set_name;
        doc.next_set_id=manifest.next_set_id;
        for set in manifest.texture_sets {
            let layers=load_layers(&mut zip,Some(set.id),set.size,set.layers)?;
            doc.texture_sets.push(TextureSet {
                id:set.id,name:set.name,size:set.size,layers,
                selected:set.selected,next_layer_id:set.next_layer_id,
            });
        }
    }
    doc.validate()?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Luma,Rgba};

    fn path(label:&str)->std::path::PathBuf {
        std::env::temp_dir().join(format!("kalium-{label}-{}-{}.kalium",std::process::id(),
            std::thread::current().name().unwrap_or("test").replace(':',"_")))
    }

    #[test]
    fn project_round_trip_keeps_editable_layers() {
        let mut doc=Document::new(64);
        doc.add_layer().unwrap();
        doc.selected_layer_mut().mask.put_pixel(12,12,Luma([70]));
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64).put_pixel(11,10,Rgba([12,34,56,200]));
        doc.selected_layer_mut().disabled_channels.insert(Channel::BaseColor);
        let path=path("roundtrip");
        save(&doc,&path).unwrap();
        let loaded=load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.layers.len(),2);
        assert!(loaded.selected_layer().disabled_channels.contains(&Channel::BaseColor));
        assert_eq!(loaded.layers[1].mask.get_pixel(12,12).0,[70]);
        assert_eq!(loaded.layers[1].pixels[&Channel::BaseColor].get_pixel(11,10).0,[12,34,56,200]);
    }

    #[test]
    fn multiple_texture_sets_roundtrip_with_distinct_layers() {
        let mut doc=Document::new(64);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64)
            .put_pixel(0,0,Rgba([1,2,3,255]));
        let id=doc.add_texture_set("Armor / Leather").unwrap();
        doc.resize(128);
        doc.selected_layer_mut().mask.put_pixel(1,1,Luma([64]));
        doc.selected_layer_mut().pixels_mut(Channel::Metallic,128)
            .put_pixel(0,0,Rgba([9,8,7,255]));
        let path=path("sets");
        save(&doc,&path).unwrap();
        let mut restored=load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(restored.texture_set_count(),2);
        assert_eq!(restored.active_set_id,id);
        assert_eq!(restored.active_set_name,"Armor / Leather");
        assert_eq!(restored.size,128);
        assert_eq!(restored.selected_layer().mask.get_pixel(1,1).0,[64]);
        restored.activate_texture_set(1).unwrap();
        assert_eq!(restored.size,64);
        assert_eq!(restored.composite(Channel::BaseColor).get_pixel(0,0).0,[1,2,3,255]);
        restored.validate().unwrap();
    }

    #[test]
    fn mesh_material_bindings_roundtrip_v4() {
        let mut doc=Document::new(64);
        let second=doc.add_texture_set("Leather").unwrap();
        doc.mesh.materials.push("Glove".into());
        doc.mesh.triangle_materials[0]=1;
        doc.material_bindings=vec![Some(second),None];
        let path=path("material-bindings");
        save(&doc,&path).unwrap();
        let loaded=load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.material_bindings,vec![Some(second),None]);
        assert_eq!(loaded.mesh.triangle_material(0),1);
        assert_eq!(loaded.mesh.material_name(1),"Glove");
    }
    #[test]
    fn legacy_v1_manifest_defaults_to_enabled_channels() {
        let json=r#"{"id":1,"name":"Legacy","visible":true,"opacity":1.0,"blend":"Normal","channels":["BaseColor"]}"#;
        let layer:LayerManifest=serde_json::from_str(json).unwrap();
        assert!(layer.disabled_channels.is_empty());
    }

    #[test]
    fn bundled_v1_demo_remains_loadable() {
        let path=Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Kalium-Demo.kalium");
        let doc=load(&path).unwrap();
        assert!(doc.layers.iter().all(|l|l.disabled_channels.is_empty()));
        assert_eq!(doc.texture_set_count(),1);
        assert_eq!(doc.active_set_id,1);
        doc.validate().unwrap();
    }
}

//! Native .kalium projects are versioned ZIP containers with a JSON manifest,
//! the editable mesh, one PNG per painted channel, and PNG layer masks.
use crate::document::{BlendMode,Channel,Document,Layer,MAX_LAYERS,MAX_SIZE};
use crate::mesh::Mesh;
use anyhow::{anyhow,bail,Context,Result};
use serde::{Deserialize,Serialize};
use std::{fs::File,io::{Read,Write},path::Path};
use zip::{ZipArchive,ZipWriter,write::FileOptions,CompressionMethod};

#[derive(Serialize,Deserialize)]
struct Manifest {
    format:String,
    version:u32,
    size:u32,
    mesh:Mesh,
    selected:usize,
    next_layer_id:u32,
    layers:Vec<LayerManifest>,
}
#[derive(Serialize,Deserialize)]
struct LayerManifest {
    id:u32,
    name:String,
    visible:bool,
    opacity:f32,
    blend:BlendMode,
    channels:Vec<Channel>,
}

pub fn save(doc:&Document,path:&Path)->Result<()> {
    doc.validate()?;
    let manifest=Manifest {
        format:"Kalium Project".into(),version:1,size:doc.size,mesh:doc.mesh.clone(),
        selected:doc.selected,next_layer_id:doc.next_layer_id,
        layers:doc.layers.iter().map(|l|LayerManifest {
            id:l.id,name:l.name.clone(),visible:l.visible,opacity:l.opacity,blend:l.blend,
            channels:Channel::ALL.into_iter().filter(|c|l.pixels.contains_key(c)).collect(),
        }).collect(),
    };
    let filename=path.file_name().ok_or_else(||anyhow!("Invalid save filename"))?;
    let mut temp_name=filename.to_os_string();
    temp_name.push(".tmp");
    let tmp=path.with_file_name(temp_name);
    let result=(|| -> Result<()> {
        let file=File::create(&tmp)?;
        let mut writer=ZipWriter::new(file);
        let opts=FileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file("manifest.json",opts)?;
        writer.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        for layer in &doc.layers {
            writer.start_file(format!("layers/{}/mask.png",layer.id),opts)?;
            let image=image::DynamicImage::ImageLuma8(layer.mask.clone());
            let mut buf=std::io::Cursor::new(Vec::<u8>::new());
            image.write_to(&mut buf,image::ImageFormat::Png)?;
            writer.write_all(&buf.into_inner())?;
            for (channel,pixels) in &layer.pixels {
                writer.start_file(format!("layers/{}/{}.png",layer.id,channel.filename()),opts)?;
                let mut buf=std::io::Cursor::new(Vec::<u8>::new());
                image::DynamicImage::ImageRgba8(pixels.clone())
                    .write_to(&mut buf,image::ImageFormat::Png)?;
                writer.write_all(&buf.into_inner())?;
            }
        }
        writer.finish()?;
        Ok(())
    })();
    if let Err(e)=result {let _=std::fs::remove_file(&tmp);return Err(e);}
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

pub fn load(path:&Path)->Result<Document> {
    let file=File::open(path)?;
    let mut zip=ZipArchive::new(file).context("Not a valid Kalium project ZIP")?;
    let manifest:Manifest=serde_json::from_slice(&read_entry(&mut zip,"manifest.json",48*1024*1024)?)?;
    if manifest.format!="Kalium Project" || manifest.version!=1 {bail!("Unsupported Kalium project version");}
    if !(64..=MAX_SIZE).contains(&manifest.size) {bail!("Invalid texture dimension");}
    if manifest.layers.is_empty()||manifest.layers.len()>MAX_LAYERS {bail!("Invalid layer count");}
    manifest.mesh.validate()?;
    let mut doc=Document {
        mesh:manifest.mesh,size:manifest.size,
        layers:Vec::new(), selected:manifest.selected,next_layer_id:manifest.next_layer_id,
    };
    let pixels_bytes=(doc.size as u64)*(doc.size as u64)*4;
    for m in manifest.layers {
        let mut layer=Layer::new(m.id,m.name,doc.size);
        layer.visible=m.visible;layer.opacity=m.opacity;layer.blend=m.blend;
        let mask=read_entry(&mut zip,&format!("layers/{}/mask.png",m.id),pixels_bytes+4*1024*1024)?;
        let decoded=image::load_from_memory(&mask)?.to_luma8();
        if decoded.dimensions()!=(doc.size,doc.size) {bail!("Mask dimensions do not match project");}
        layer.mask=decoded;
        for channel in m.channels {
            let bytes=read_entry(&mut zip,&format!("layers/{}/{}.png",m.id,channel.filename()),pixels_bytes+4*1024*1024)?;
            let rgba=image::load_from_memory(&bytes)?.to_rgba8();
            if rgba.dimensions()!=(doc.size,doc.size) {bail!("Layer texture dimensions do not match project");}
            layer.pixels.insert(channel,rgba);
        }
        doc.layers.push(layer);
    }
    doc.validate()?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Luma,Rgba};
    #[test]
    fn project_round_trip_keeps_editable_layers() {
        let mut doc=Document::new(64);
        doc.add_layer().unwrap();
        doc.selected_layer_mut().mask.put_pixel(12,12,Luma([70]));
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64).put_pixel(11,10,Rgba([12,34,56,200]));
        let path=std::env::temp_dir().join(format!("kalium-test-{}.kalium",std::process::id()));
        save(&doc,&path).unwrap();
        let loaded=load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.layers.len(),2);
        assert_eq!(loaded.layers[1].mask.get_pixel(12,12).0,[70]);
        assert_eq!(loaded.layers[1].pixels[&Channel::BaseColor].get_pixel(11,10).0,[12,34,56,200]);
    }
}

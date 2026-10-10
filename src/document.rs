use crate::mesh::Mesh;
use anyhow::{bail, Result};
use image::{GrayImage, ImageBuffer, Luma, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap,HashSet};

pub const MAX_SIZE: u32 = 4096;
pub const MAX_LAYERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Channel {
    BaseColor, Roughness, Metallic, Normal, Height, Emissive, Opacity, Occlusion,
}

impl Channel {
    pub const ALL: [Self;8] = [Self::BaseColor,Self::Roughness,Self::Metallic,Self::Normal,
        Self::Height,Self::Emissive,Self::Opacity,Self::Occlusion];
    pub fn label(self) -> &'static str {
        match self {
            Self::BaseColor=>"Base Color",Self::Roughness=>"Roughness",Self::Metallic=>"Metallic",
            Self::Normal=>"Normal",Self::Height=>"Height",Self::Emissive=>"Emissive",
            Self::Opacity=>"Opacity",Self::Occlusion=>"Ambient Occlusion",
        }
    }
    pub fn filename(self) -> &'static str {
        match self {
            Self::BaseColor=>"BaseColor",Self::Roughness=>"Roughness",Self::Metallic=>"Metallic",
            Self::Normal=>"Normal",Self::Height=>"Height",Self::Emissive=>"Emissive",
            Self::Opacity=>"Opacity",Self::Occlusion=>"AmbientOcclusion",
        }
    }
    pub fn background(self) -> [u8;4] {
        match self {
            Self::BaseColor=>[190,185,202,255], Self::Roughness=>[170,170,170,255],
            Self::Metallic=>[0,0,0,255], Self::Normal=>[128,128,255,255],
            Self::Height=>[128,128,128,255], Self::Emissive=>[0,0,0,255],
            Self::Opacity|Self::Occlusion=>[255,255,255,255],
        }
    }
    pub fn is_color(self) -> bool {
        matches!(self,Self::BaseColor|Self::Emissive|Self::Normal)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlendMode { Normal, Multiply, Add }

impl BlendMode { pub const ALL: [Self;3] = [Self::Normal,Self::Multiply,Self::Add]; }

pub struct Layer {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub blend: BlendMode,
    pub pixels: HashMap<Channel,RgbaImage>,
    pub mask: GrayImage,
}

impl Layer {
    pub fn new(id:u32,name:impl Into<String>,size:u32)->Self {
        Self {id,name:name.into(),visible:true,opacity:1.0,blend:BlendMode::Normal,
            pixels:HashMap::new(),mask:ImageBuffer::from_pixel(size,size,Luma([255]))}
    }
    pub fn pixels_mut(&mut self,channel:Channel,size:u32)->&mut RgbaImage {
        self.pixels.entry(channel).or_insert_with(||RgbaImage::new(size,size))
    }
}

pub struct Document {
    pub mesh:Mesh,
    pub size:u32,
    pub layers:Vec<Layer>,
    pub selected:usize,
    pub next_layer_id:u32,
}

impl Document {
    pub fn new(size:u32)->Self {
        let size=size.clamp(64,MAX_SIZE);
        Self {mesh:Mesh::demo_cube(),size,layers:vec![Layer::new(1,"Paint Layer",size)],
            selected:0,next_layer_id:2}
    }
    pub fn selected_layer(&self)->&Layer {&self.layers[self.selected]}
    pub fn selected_layer_mut(&mut self)->&mut Layer {&mut self.layers[self.selected]}
    pub fn add_layer(&mut self)->Result<()> {
        if self.layers.len()>=MAX_LAYERS {bail!("Maximum number of layers reached");}
        let id=self.next_layer_id;
        self.next_layer_id=self.next_layer_id.saturating_add(1);
        let layer=Layer::new(id,format!("Paint Layer {}",id),self.size);
        self.selected+=1;
        self.layers.insert(self.selected,layer);
        Ok(())
    }
    pub fn remove_layer(&mut self) {
        if self.layers.len()>1 {self.layers.remove(self.selected);self.selected=self.selected.min(self.layers.len()-1);}
    }
    pub fn move_selected(&mut self,up:bool) {
        let dst = if up {self.selected.checked_add(1).filter(|i|*i<self.layers.len())}
                  else {self.selected.checked_sub(1)};
        if let Some(dst)=dst {self.layers.swap(self.selected,dst);self.selected=dst;}
    }
    pub fn resize(&mut self,size:u32) {
        if size==self.size || !(64..=MAX_SIZE).contains(&size) {return;}
        for layer in &mut self.layers {
            layer.mask = image::imageops::resize(&layer.mask,size,size,image::imageops::FilterType::Nearest);
            for pixels in layer.pixels.values_mut() {
                *pixels = image::imageops::resize(pixels,size,size,image::imageops::FilterType::Nearest);
            }
        }
        self.size=size;
    }
    pub fn composite(&self,channel:Channel)->RgbaImage {
        let size=self.size;
        let mut out=ImageBuffer::from_pixel(size,size,Rgba(channel.background()));
        for layer in &self.layers {
            if !layer.visible || layer.opacity<=0.0 {continue;}
            let Some(src)=layer.pixels.get(&channel) else {continue};
            let data=src.as_raw();
            let masks=layer.mask.as_raw();
            let dst=out.as_mut();
            for (i, dst_pixel) in dst.chunks_exact_mut(4).enumerate() {
                let s=&data[i*4..i*4+4];
                let a=(s[3] as f32/255.0)*layer.opacity*(masks[i] as f32/255.0);
                if a<=0.0 {continue;}
                for c in 0..3 {
                    let source=s[c] as f32;
                    let d=dst_pixel[c] as f32;
                    let blended=match layer.blend {
                        BlendMode::Normal=>source,
                        BlendMode::Multiply=>d*source/255.0,
                        BlendMode::Add=>(d+source).min(255.0),
                    };
                    dst_pixel[c]=(d+(blended-d)*a).round().clamp(0.0,255.0) as u8;
                }
            }
        }
        out
    }
    pub fn validate(&self)->Result<()> {
        self.mesh.validate()?;
        if !(64..=MAX_SIZE).contains(&self.size) {bail!("Texture dimension is invalid");}
        if self.layers.is_empty() || self.layers.len()>MAX_LAYERS {bail!("Invalid layer count");}
        if self.selected>=self.layers.len() {bail!("Invalid selected layer");}
        let mut ids=HashSet::new();
        for layer in &self.layers {
            if !ids.insert(layer.id) {bail!("Duplicate layer id");}
            if layer.mask.dimensions()!=(self.size,self.size) {bail!("Invalid layer mask size");}
            if !layer.opacity.is_finite() || !(0.0..=1.0).contains(&layer.opacity) {bail!("Invalid layer opacity");}
            for image in layer.pixels.values() {
                if image.dimensions()!=(self.size,self.size) {bail!("Invalid layer image dimensions");}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blank_layers_preserve_background() {
        let doc=Document::new(64);
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(10,10).0,Channel::BaseColor.background());
    }
    #[test]
    fn painted_layer_blends_with_background() {
        let mut doc=Document::new(64);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64).put_pixel(4,4,Rgba([255,0,0,255]));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,4).0,[255,0,0,255]);
        doc.selected_layer_mut().mask.put_pixel(4,4,Luma([0]));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,4).0,Channel::BaseColor.background());
    }
}

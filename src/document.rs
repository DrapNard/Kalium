use crate::mesh::Mesh;
use anyhow::{bail, Result};
use image::{GrayImage, ImageBuffer, Luma, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap,HashSet};

pub const MAX_SIZE: u32 = 4096;
pub const MAX_LAYERS: usize = 64;
pub const MAX_TEXTURE_SETS: usize = 16;
pub const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

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

#[derive(Clone)]
pub struct Layer {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub blend: BlendMode,
    pub pixels: HashMap<Channel,RgbaImage>,
    /// Disabled PBR channels remain editable and stored, but are skipped by compositing.
    pub disabled_channels: HashSet<Channel>,
    pub mask: GrayImage,
}

impl Layer {
    pub fn new(id: u32, name: impl Into<String>, size: u32) -> Self {
        Self {
            id,
            name: name.into(),
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            pixels: HashMap::new(),
            disabled_channels: HashSet::new(),
            mask: ImageBuffer::from_pixel(size, size, Luma([255])),
        }
    }
    pub fn pixels_mut(&mut self,channel:Channel,size:u32)->&mut RgbaImage {
        self.pixels.entry(channel).or_insert_with(||RgbaImage::new(size,size))
    }
}

/// A complete inactive texture set. The active set lives in the legacy Document
/// fields, so existing brush, history, and viewport code operates on the active
/// set without cloning potentially hundreds of megabytes on each switch.
pub struct TextureSet {
    pub id: u32,
    pub name: String,
    pub size: u32,
    pub layers: Vec<Layer>,
    pub selected: usize,
    pub next_layer_id: u32,
}

pub struct Document {
    pub mesh: Mesh,
    pub size: u32,
    pub layers: Vec<Layer>,
    pub selected: usize,
    pub next_layer_id: u32,
    pub active_set_id: u32,
    pub active_set_name: String,
    pub next_set_id: u32,
    /// These exclude the active set. Switching uses ownership transfer, not clones.
    pub texture_sets: Vec<TextureSet>,
    /// Index by mesh material slot. A missing binding uses the active set.
    pub material_bindings: Vec<Option<u32>>,
}

impl Document {
    pub fn new(size:u32)->Self {
        let size=size.clamp(64,MAX_SIZE);
        Self {mesh:Mesh::demo_cube(),size,layers:vec![Layer::new(1,"Paint Layer",size)],
            selected:0,next_layer_id:2,active_set_id:1,active_set_name:"Default".into(),
            next_set_id:2,texture_sets:Vec::new(),material_bindings:vec![None]}
    }
    pub fn texture_set_count(&self) -> usize { 1 + self.texture_sets.len() }

    /// A material with no explicit binding uses the active set (legacy behavior).
    pub fn texture_set_for_material(&self, material_slot:usize) -> u32 {
        self.material_bindings.get(material_slot).copied().flatten().unwrap_or(self.active_set_id)
    }

    pub fn bind_material(&mut self, material_slot:usize, set:Option<u32>) -> Result<()> {
        if material_slot >= self.mesh.material_count() { bail!("Unknown mesh material slot"); }
        if let Some(id)=set {
            if id!=self.active_set_id && !self.texture_sets.iter().any(|item|item.id==id) {
                bail!("Unknown texture set {id}");
            }
        }
        self.material_bindings.resize(self.mesh.material_count(),None);
        self.material_bindings[material_slot]=set;
        Ok(())
    }

    pub fn import_mesh(&mut self, mesh:Mesh) -> Result<()> {
        mesh.validate()?;
        self.mesh=mesh;
        // Replacing a mesh never borrows old slot positions. Keep all newly
        // imported materials pinned to the current set until explicitly remapped.
        self.material_bindings=vec![Some(self.active_set_id);self.mesh.material_count()];
        Ok(())
    }


    pub fn texture_set_list(&self) -> Vec<(u32, String, u32, bool)> {
        let mut list = vec![(self.active_set_id, self.active_set_name.clone(), self.size, true)];
        list.extend(self.texture_sets.iter().map(|set| (set.id, set.name.clone(), set.size, false)));
        list.sort_by_key(|item| item.0);
        list
    }

    /// Takes ownership of the active layers without cloning pixel buffers.
    pub fn activate_texture_set(&mut self, id: u32) -> Result<bool> {
        if id == self.active_set_id { return Ok(false); }
        let index = self.texture_sets.iter().position(|set| set.id == id)
            .ok_or_else(||anyhow::anyhow!("Unknown texture set {id}"))?;
        let target = self.texture_sets.swap_remove(index);
        let previous = TextureSet {
            id: self.active_set_id, name: std::mem::take(&mut self.active_set_name),
            size: self.size, layers: std::mem::take(&mut self.layers),
            selected: self.selected, next_layer_id: self.next_layer_id,
        };
        self.texture_sets.push(previous);
        self.active_set_id = target.id;
        self.active_set_name = target.name;
        self.size = target.size;
        self.layers = target.layers;
        self.selected = target.selected;
        self.next_layer_id = target.next_layer_id;
        Ok(true)
    }

    pub fn add_texture_set(&mut self, name: &str) -> Result<u32> {
        if self.texture_set_count() >= MAX_TEXTURE_SETS { bail!("Maximum number of texture sets reached"); }
        let name = name.trim();
        if name.is_empty() || name.len() > 128 { bail!("Texture set name must contain 1–128 bytes"); }
        let id = self.next_set_id;
        let next = id.checked_add(1).ok_or_else(||anyhow::anyhow!("Texture set ID counter exhausted"))?;
        let mut projected = self.decoded_bytes();
        projected = projected.saturating_add((self.size as u64).pow(2));
        if projected > MAX_DECODED_BYTES { bail!("Texture sets exceed 512-MiB decoded texture budget"); }
        let new_set = TextureSet {
            id, name: name.to_owned(), size: self.size,
            layers: vec![Layer::new(1,"Paint Layer",self.size)],
            selected: 0, next_layer_id: 2,
        };
        self.texture_sets.push(new_set);
        self.next_set_id = next;
        self.activate_texture_set(id)?;
        Ok(id)
    }

    pub fn remove_active_texture_set(&mut self) -> Result<()> {
        if self.texture_sets.is_empty() { bail!("A project must keep at least one texture set"); }
        let removed_id = self.active_set_id;
        // The lowest surviving id is chosen predictably.
        let id = self.texture_sets.iter().map(|set|set.id).min()
            .ok_or_else(||anyhow::anyhow!("No other texture set"))?;
        self.activate_texture_set(id)?;
        self.texture_sets.retain(|set| set.id != removed_id);
        for binding in &mut self.material_bindings {
            if *binding==Some(removed_id) { *binding=None; }
        }
        Ok(())
    }

    pub fn rename_active_texture_set(&mut self, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 128 { bail!("Texture set name must contain 1–128 bytes"); }
        self.active_set_name = name.to_owned();
        Ok(())
    }

    pub fn decoded_bytes(&self) -> u64 {
        fn used(layers: &[Layer]) -> u64 {
            layers.iter().fold(0u64, |sum, layer| sum.saturating_add(
                layer.mask.len() as u64 + layer.pixels.values().map(|p|p.len() as u64).sum::<u64>()))
        }
        self.texture_sets.iter().fold(used(&self.layers), |sum,set| sum.saturating_add(used(&set.layers)))
    }

    pub fn composite_for_set(&self, id: u32, channel: Channel) -> Option<RgbaImage> {
        if id == self.active_set_id { return Some(self.composite(channel)); }
        self.texture_sets.iter().find(|set|set.id==id).map(|set|composite_layers(&set.layers,set.size,channel))
    }

    pub fn selected_layer(&self)->&Layer {&self.layers[self.selected]}
    pub fn selected_layer_mut(&mut self)->&mut Layer {&mut self.layers[self.selected]}
    pub fn add_layer(&mut self)->Result<()> {
        if self.layers.len()>=MAX_LAYERS {bail!("Maximum number of layers reached");}
        let id=self.next_layer_id;
        self.next_layer_id=self.next_layer_id.checked_add(1)
            .ok_or_else(||anyhow::anyhow!("Layer ID counter exhausted"))?;
        let layer=Layer::new(id,format!("Paint Layer {}",id),self.size);
        self.selected+=1;
        self.layers.insert(self.selected,layer);
        Ok(())
    }
    /// Duplicate the selected layer with all of its PBR channels and its mask.
    /// The copy receives a fresh ID, so edits/history cannot collide.
    pub fn duplicate_selected(&mut self)->Result<()> {
        if self.layers.len()>=MAX_LAYERS {bail!("Maximum number of layers reached");}
        let id=self.next_layer_id;
        self.next_layer_id=self.next_layer_id.checked_add(1)
            .ok_or_else(||anyhow::anyhow!("Layer ID counter exhausted"))?;
        // Avoid allocating a full second copy of a multi-channel 4K layer
        // when it would exceed the same 512-MiB decoded document budget
        // enforced by the native .kalium importer.
        let used=self.decoded_bytes();
        let selected=self.selected_layer();
        let extra=(selected.mask.len() as u64).saturating_add(
            selected.pixels.values().map(|image|image.len() as u64).sum::<u64>());
        if used.saturating_add(extra)>512*1024*1024 {
            bail!("Duplicating this layer would exceed the 512-MiB texture budget");
        }
        let mut copy=selected.clone();
        copy.id=id;
        copy.name=format!("{} copy",copy.name);
        self.selected+=1;
        self.layers.insert(self.selected,copy);
        Ok(())
    }
    /// Inversion is an edit on a persistent grayscale layer mask.
    pub fn invert_selected_mask(&mut self)->bool {
        let mask=&mut self.selected_layer_mut().mask;
        let dirty=!mask.is_empty();
        for pixel in mask.pixels_mut() {pixel.0[0]=255-pixel.0[0];}
        dirty
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
    pub fn composite(&self, channel: Channel) -> RgbaImage {
        composite_layers(&self.layers,self.size,channel)
    }
    pub fn validate(&self)->Result<()> {
        self.mesh.validate()?;
        if self.material_bindings.len() != self.mesh.material_count() {
            bail!("Material binding count does not match imported mesh slots");
        }
        for id in self.material_bindings.iter().flatten() {
            if *id != self.active_set_id && !self.texture_sets.iter().any(|s|s.id==*id) {
                bail!("Material binding references missing texture set {id}");
            }
        }
        if self.texture_set_count()>MAX_TEXTURE_SETS { bail!("Too many texture sets"); }
        if self.active_set_id==0 || self.next_set_id<=self.active_set_id { bail!("Invalid active texture set ID"); }
        validate_texture_set(self.size,&self.layers,self.selected,self.next_layer_id,&self.active_set_name)?;
        let mut ids=HashSet::from([self.active_set_id]);
        for set in &self.texture_sets {
            if set.id==0 || set.id>=self.next_set_id || !ids.insert(set.id) {bail!("Duplicate or invalid texture set ID");}
            validate_texture_set(set.size,&set.layers,set.selected,set.next_layer_id,&set.name)?;
        }
        if self.decoded_bytes()>MAX_DECODED_BYTES {bail!("Project exceeds 512-MiB decoded texture budget");}
        Ok(())
    }

}

fn validate_texture_set(size:u32,layers:&[Layer],selected:usize,next_layer_id:u32,name:&str)->Result<()> {
    if !(64..=MAX_SIZE).contains(&size) {bail!("Invalid texture dimension");}
    if name.trim().is_empty() || name.len()>128 {bail!("Invalid texture set name");}
    if layers.is_empty() || layers.len()>MAX_LAYERS {bail!("Invalid layer count");}
    if selected>=layers.len() {bail!("Invalid selected layer");}
    let mut ids=HashSet::new();
    if next_layer_id<=layers.iter().map(|l|l.id).max().unwrap_or(0) {bail!("Invalid next layer ID");}
    for layer in layers {
        if !ids.insert(layer.id) {bail!("Duplicate layer ID");}
        if layer.name.is_empty() || layer.name.len()>1024 {bail!("Invalid layer name");}
        if layer.mask.dimensions()!=(size,size) {bail!("Invalid layer mask size");}
        if !layer.opacity.is_finite() || !(0.0..=1.0).contains(&layer.opacity) {bail!("Invalid opacity");}
        for image in layer.pixels.values() {
            if image.dimensions()!=(size,size) {bail!("Invalid layer texture size");}
        }
    }
    Ok(())
}

fn composite_layers(layers:&[Layer],size:u32,channel:Channel)->RgbaImage {
        let mut out=ImageBuffer::from_pixel(size,size,Rgba(channel.background()));
        for layer in layers {
            if !layer.visible || layer.opacity<=0.0 {continue;}
            if layer.disabled_channels.contains(&channel) {continue;}
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


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn importing_mesh_pins_existing_set_for_all_materials() {
        let mut doc=Document::new(64);
        let mut mesh=Mesh::demo_cube();
        mesh.materials.push("Leather".into());
        mesh.triangle_materials[0]=1;
        doc.import_mesh(mesh).unwrap();
        assert_eq!(doc.material_bindings,vec![Some(1),Some(1)]);
        doc.add_texture_set("Metal").unwrap();
        assert_eq!(doc.texture_set_for_material(0),1);
        assert_eq!(doc.texture_set_for_material(1),1);
    }
    #[test]
    fn material_bindings_survive_set_switch_and_removal() {
        let mut doc=Document::new(64);
        doc.mesh.materials.push("Leather".to_owned());
        doc.mesh.triangle_materials[0]=1;
        doc.material_bindings.push(None);
        let metal=doc.add_texture_set("Metal").unwrap();
        doc.bind_material(0,Some(metal)).unwrap();
        doc.activate_texture_set(1).unwrap();
        assert_eq!(doc.texture_set_for_material(0),metal);
        assert_eq!(doc.texture_set_for_material(1),1);
        doc.activate_texture_set(metal).unwrap();
        doc.remove_active_texture_set().unwrap();
        assert_eq!(doc.material_bindings[0],None);
        doc.validate().unwrap();
    }
    #[test]
    fn layer_constructor_initializes_empty_channels_and_opaque_mask() {
        let layer = Layer::new(99, "Fresh Paint", 64);
        assert_eq!(layer.id, 99);
        assert_eq!(layer.name, "Fresh Paint");
        assert!(layer.visible);
        assert_eq!(layer.opacity, 1.0);
        assert_eq!(layer.blend, BlendMode::Normal);
        assert!(layer.pixels.is_empty());
        assert!(layer.disabled_channels.is_empty());
        assert_eq!(layer.mask.dimensions(), (64, 64));
        assert!(layer.mask.pixels().all(|pixel| pixel.0[0] == 255));
    }

    #[test]
    fn blank_layers_preserve_background() {
        let doc=Document::new(64);
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(10,10).0,Channel::BaseColor.background());
    }
    #[test]
    fn duplicate_preserves_channels_and_mask_but_creates_new_id() {
        let mut doc=Document::new(64);
        doc.selected_layer_mut().pixels_mut(Channel::Metallic,64)
            .put_pixel(2,3,Rgba([123,123,123,255]));
        doc.selected_layer_mut().mask.put_pixel(2,3,Luma([45]));
        doc.duplicate_selected().unwrap();
        assert_eq!(doc.layers.len(),2);
        assert_eq!(doc.selected,1);
        assert_ne!(doc.layers[0].id,doc.layers[1].id);
        assert_eq!(doc.layers[1].mask.get_pixel(2,3).0,[45]);
        assert_eq!(doc.layers[1].pixels[&Channel::Metallic].get_pixel(2,3).0,[123,123,123,255]);
        doc.layers[1].pixels_mut(Channel::Metallic,64).put_pixel(2,3,Rgba([1,1,1,255]));
        assert_eq!(doc.layers[0].pixels[&Channel::Metallic].get_pixel(2,3).0,[123,123,123,255]);
        doc.invert_selected_mask();
        assert_eq!(doc.layers[1].mask.get_pixel(2,3).0,[210]);
        doc.layers[1].disabled_channels.insert(Channel::Metallic);
        assert!(!doc.layers[0].disabled_channels.contains(&Channel::Metallic));
        doc.validate().unwrap();
    }
    #[test]
    fn painted_layer_blends_with_background() {
        let mut doc=Document::new(64);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64).put_pixel(4,4,Rgba([255,0,0,255]));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,4).0,[255,0,0,255]);
        doc.selected_layer_mut().mask.put_pixel(4,4,Luma([0]));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,4).0,Channel::BaseColor.background());
    }
    #[test]
    fn disabled_channel_keeps_source_but_skips_compositing() {
        let mut doc=Document::new(64);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64)
            .put_pixel(4,5,Rgba([24,38,52,255]));
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,5).0,[24,38,52,255]);
        doc.selected_layer_mut().disabled_channels.insert(Channel::BaseColor);
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,5).0,Channel::BaseColor.background());
        assert_eq!(doc.selected_layer().pixels[&Channel::BaseColor].get_pixel(4,5).0,[24,38,52,255]);
        doc.selected_layer_mut().disabled_channels.remove(&Channel::BaseColor);
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(4,5).0,[24,38,52,255]);
    }

    #[test]
    fn texture_sets_keep_independent_layers_and_resolutions() {
        let mut doc=Document::new(64);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,64).put_pixel(1,1,Rgba([200,0,0,255]));
        let second=doc.add_texture_set("Armor").unwrap();
        assert_eq!(doc.active_set_id,second);
        assert_eq!(doc.texture_set_count(),2);
        doc.resize(128);
        doc.selected_layer_mut().pixels_mut(Channel::BaseColor,128).put_pixel(1,1,Rgba([0,200,0,255]));
        doc.activate_texture_set(1).unwrap();
        assert_eq!(doc.size,64);
        assert_eq!(doc.composite(Channel::BaseColor).get_pixel(1,1).0,[200,0,0,255]);
        assert_eq!(doc.composite_for_set(second,Channel::BaseColor).unwrap().get_pixel(1,1).0,[0,200,0,255]);
        doc.activate_texture_set(second).unwrap();
        assert_eq!(doc.size,128);
        doc.validate().unwrap();
        doc.remove_active_texture_set().unwrap();
        assert_eq!(doc.texture_set_count(),1);
        assert_eq!(doc.active_set_id,1);
    }

}

use crate::mesh::Mesh;
use anyhow::{bail, Result};
use image::{GrayImage, ImageBuffer, Luma, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const MAX_SIZE: u32 = 4096;
pub const MAX_LAYERS: usize = 64;
pub const MAX_TEXTURE_SETS: usize = 16;
pub const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Channel {
    BaseColor,
    Roughness,
    Metallic,
    Normal,
    Height,
    Emissive,
    Opacity,
    Occlusion,
}

impl Channel {
    pub const ALL: [Self; 8] = [
        Self::BaseColor,
        Self::Roughness,
        Self::Metallic,
        Self::Normal,
        Self::Height,
        Self::Emissive,
        Self::Opacity,
        Self::Occlusion,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::BaseColor => "Base Color",
            Self::Roughness => "Roughness",
            Self::Metallic => "Metallic",
            Self::Normal => "Normal",
            Self::Height => "Height",
            Self::Emissive => "Emissive",
            Self::Opacity => "Opacity",
            Self::Occlusion => "Ambient Occlusion",
        }
    }
    pub fn filename(self) -> &'static str {
        match self {
            Self::BaseColor => "BaseColor",
            Self::Roughness => "Roughness",
            Self::Metallic => "Metallic",
            Self::Normal => "Normal",
            Self::Height => "Height",
            Self::Emissive => "Emissive",
            Self::Opacity => "Opacity",
            Self::Occlusion => "AmbientOcclusion",
        }
    }
    pub fn background(self) -> [u8; 4] {
        match self {
            Self::BaseColor => [190, 185, 202, 255],
            Self::Roughness => [170, 170, 170, 255],
            Self::Metallic => [0, 0, 0, 255],
            Self::Normal => [128, 128, 255, 255],
            Self::Height => [128, 128, 128, 255],
            Self::Emissive => [0, 0, 0, 255],
            Self::Opacity | Self::Occlusion => [255, 255, 255, 255],
        }
    }
    pub fn is_color(self) -> bool {
        matches!(self, Self::BaseColor | Self::Emissive | Self::Normal)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlendMode {
    Normal,
    Multiply,
    Add,
    /// Reoriented normal-map composition; other PBR channels treat this as Normal.
    Detail,
}

impl BlendMode {
    pub const ALL: [Self; 4] = [Self::Normal, Self::Multiply, Self::Add, Self::Detail];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Multiply => "Multiply",
            Self::Add => "Add",
            Self::Detail => "Detail Normal",
        }
    }
}

#[derive(Clone)]
pub struct Layer {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub blend: BlendMode,
    pub pixels: HashMap<Channel, RgbaImage>,
    /// Disabled PBR channels remain editable and stored, but are skipped by compositing.
    pub disabled_channels: HashSet<Channel>,
    pub mask: GrayImage,
    /// Unknown manifest fields retained for forward-compatible project round trips.
    pub(crate) extensions: JsonMap<String, JsonValue>,
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
            extensions: JsonMap::new(),
        }
    }
    pub fn pixels_mut(&mut self, channel: Channel, size: u32) -> &mut RgbaImage {
        self.pixels
            .entry(channel)
            .or_insert_with(|| RgbaImage::new(size, size))
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
    /// Unknown manifest fields retained for forward-compatible project round trips.
    pub(crate) extensions: JsonMap<String, JsonValue>,
}

pub struct Document {
    /// Stable identity used to bind recovery journals to this project.
    pub project_id: Uuid,
    pub mesh: Mesh,
    pub size: u32,
    pub layers: Vec<Layer>,
    pub selected: usize,
    pub next_layer_id: u32,
    pub active_set_id: u32,
    pub active_set_name: String,
    /// Unknown fields belonging to the selected set. v1-v4 serialize these at
    /// the manifest root while this set is active.
    pub(crate) active_set_extensions: JsonMap<String, JsonValue>,
    pub next_set_id: u32,
    /// These exclude the active set. Switching uses ownership transfer, not clones.
    pub texture_sets: Vec<TextureSet>,
    /// Unknown project-level manifest fields retained across project saves.
    pub(crate) extensions: JsonMap<String, JsonValue>,
    /// Index by mesh material slot. A missing binding uses the active set.
    pub material_bindings: Vec<Option<u32>>,
}

impl Document {
    pub fn new(size: u32) -> Self {
        let size = size.clamp(64, MAX_SIZE);
        Self {
            project_id: Uuid::new_v4(),
            mesh: Mesh::demo_cube(),
            size,
            layers: vec![Layer::new(1, "Paint Layer", size)],
            selected: 0,
            next_layer_id: 2,
            active_set_id: 1,
            active_set_name: "Default".into(),
            active_set_extensions: JsonMap::new(),
            next_set_id: 2,
            texture_sets: Vec::new(),
            extensions: JsonMap::new(),
            material_bindings: vec![None],
        }
    }
    pub fn texture_set_count(&self) -> usize {
        1 + self.texture_sets.len()
    }

    /// A material with no explicit binding uses the active set (legacy behavior).
    pub fn texture_set_for_material(&self, material_slot: usize) -> u32 {
        self.material_bindings
            .get(material_slot)
            .copied()
            .flatten()
            .unwrap_or(self.active_set_id)
    }

    pub fn bind_material(&mut self, material_slot: usize, set: Option<u32>) -> Result<()> {
        if material_slot >= self.mesh.material_count() {
            bail!("Unknown mesh material slot");
        }
        if let Some(id) = set {
            if id != self.active_set_id && !self.texture_sets.iter().any(|item| item.id == id) {
                bail!("Unknown texture set {id}");
            }
        }
        self.material_bindings
            .resize(self.mesh.material_count(), None);
        self.material_bindings[material_slot] = set;
        Ok(())
    }

    pub fn import_mesh(&mut self, mesh: Mesh) -> Result<()> {
        mesh.validate()?;
        self.mesh = mesh;
        // Replacing a mesh never borrows old slot positions. Keep all newly
        // imported materials pinned to the current set until explicitly remapped.
        self.material_bindings = vec![Some(self.active_set_id); self.mesh.material_count()];
        Ok(())
    }

    pub fn texture_set_list(&self) -> Vec<(u32, String, u32, bool)> {
        let mut list = vec![(
            self.active_set_id,
            self.active_set_name.clone(),
            self.size,
            true,
        )];
        list.extend(
            self.texture_sets
                .iter()
                .map(|set| (set.id, set.name.clone(), set.size, false)),
        );
        list.sort_by_key(|item| item.0);
        list
    }

    /// Takes ownership of the active layers without cloning pixel buffers.
    pub fn activate_texture_set(&mut self, id: u32) -> Result<bool> {
        if id == self.active_set_id {
            return Ok(false);
        }
        let index = self
            .texture_sets
            .iter()
            .position(|set| set.id == id)
            .ok_or_else(|| anyhow::anyhow!("Unknown texture set {id}"))?;
        let target = self.texture_sets.swap_remove(index);
        let previous = TextureSet {
            id: self.active_set_id,
            name: std::mem::take(&mut self.active_set_name),
            size: self.size,
            layers: std::mem::take(&mut self.layers),
            selected: self.selected,
            next_layer_id: self.next_layer_id,
            extensions: std::mem::take(&mut self.active_set_extensions),
        };
        self.texture_sets.push(previous);
        self.active_set_id = target.id;
        self.active_set_name = target.name;
        self.size = target.size;
        self.layers = target.layers;
        self.selected = target.selected;
        self.next_layer_id = target.next_layer_id;
        self.active_set_extensions = target.extensions;
        Ok(true)
    }

    pub fn add_texture_set(&mut self, name: &str) -> Result<u32> {
        if self.texture_set_count() >= MAX_TEXTURE_SETS {
            bail!("Maximum number of texture sets reached");
        }
        let name = name.trim();
        if name.is_empty() || name.len() > 128 {
            bail!("Texture set name must contain 1–128 bytes");
        }
        let id = self.next_set_id;
        let next = id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Texture set ID counter exhausted"))?;
        let mut projected = self.decoded_bytes();
        projected = projected.saturating_add((self.size as u64).pow(2));
        if projected > MAX_DECODED_BYTES {
            bail!("Texture sets exceed 512-MiB decoded texture budget");
        }
        let new_set = TextureSet {
            id,
            name: name.to_owned(),
            size: self.size,
            layers: vec![Layer::new(1, "Paint Layer", self.size)],
            selected: 0,
            next_layer_id: 2,
            extensions: JsonMap::new(),
        };
        self.texture_sets.push(new_set);
        self.next_set_id = next;
        self.activate_texture_set(id)?;
        Ok(id)
    }

    pub fn remove_active_texture_set(&mut self) -> Result<()> {
        if self.texture_sets.is_empty() {
            bail!("A project must keep at least one texture set");
        }
        let removed_id = self.active_set_id;
        // The lowest surviving id is chosen predictably.
        let id = self
            .texture_sets
            .iter()
            .map(|set| set.id)
            .min()
            .ok_or_else(|| anyhow::anyhow!("No other texture set"))?;
        self.activate_texture_set(id)?;
        self.texture_sets.retain(|set| set.id != removed_id);
        for binding in &mut self.material_bindings {
            if *binding == Some(removed_id) {
                *binding = None;
            }
        }
        Ok(())
    }

    pub fn rename_active_texture_set(&mut self, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 128 {
            bail!("Texture set name must contain 1–128 bytes");
        }
        self.active_set_name = name.to_owned();
        Ok(())
    }

    pub fn decoded_bytes(&self) -> u64 {
        fn used(layers: &[Layer]) -> u64 {
            layers.iter().fold(0u64, |sum, layer| {
                let layer_bytes = layer
                    .pixels
                    .values()
                    .fold(layer.mask.len() as u64, |bytes, image| {
                        bytes.saturating_add(image.len() as u64)
                    });
                sum.saturating_add(layer_bytes)
            })
        }
        self.texture_sets
            .iter()
            .fold(used(&self.layers), |sum, set| {
                sum.saturating_add(used(&set.layers))
            })
    }

    pub fn composite_for_set(&self, id: u32, channel: Channel) -> Option<RgbaImage> {
        if id == self.active_set_id {
            return Some(self.composite(channel));
        }
        self.texture_sets
            .iter()
            .find(|set| set.id == id)
            .map(|set| composite_layers(&set.layers, set.size, channel))
    }

    /// Composites a bounded rectangle from one texture set without building the
    /// full-size composite first. Coordinates use the texture set's pixel space.
    pub fn composite_region_for_set(
        &self,
        set_id: u32,
        channel: Channel,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Option<RgbaImage> {
        let (layers, size) = if set_id == self.active_set_id {
            (&self.layers, self.size)
        } else {
            let set = self.texture_sets.iter().find(|set| set.id == set_id)?;
            (&set.layers, set.size)
        };

        if size == 0 || size > MAX_SIZE || width == 0 || height == 0 {
            return None;
        }
        let right = x.checked_add(width)?;
        let bottom = y.checked_add(height)?;
        if right > size || bottom > size {
            return None;
        }
        // Bound the output allocation even if a malformed in-memory document
        // bypassed normal size validation.
        let output_bytes = u64::from(width)
            .checked_mul(u64::from(height))?
            .checked_mul(4)?;
        if output_bytes > MAX_DECODED_BYTES {
            return None;
        }

        Some(composite_layers_region(
            layers, size, channel, x, y, width, height,
        ))
    }

    pub fn selected_layer(&self) -> &Layer {
        &self.layers[self.selected]
    }
    pub fn selected_layer_mut(&mut self) -> &mut Layer {
        &mut self.layers[self.selected]
    }
    pub fn add_layer(&mut self) -> Result<()> {
        if self.layers.len() >= MAX_LAYERS {
            bail!("Maximum number of layers reached");
        }
        let mask_bytes = u64::from(self.size)
            .checked_mul(u64::from(self.size))
            .ok_or_else(|| anyhow::anyhow!("Layer mask size overflow"))?;
        if self
            .decoded_bytes()
            .checked_add(mask_bytes)
            .is_none_or(|bytes| bytes > MAX_DECODED_BYTES)
        {
            bail!("Adding this layer would exceed the 512-MiB texture budget");
        }
        let id = self.next_layer_id;
        let next_id = self
            .next_layer_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Layer ID counter exhausted"))?;
        let layer = Layer::new(id, format!("Paint Layer {}", id), self.size);
        self.next_layer_id = next_id;
        self.selected += 1;
        self.layers.insert(self.selected, layer);
        Ok(())
    }
    /// Duplicate the selected layer with all of its PBR channels and its mask.
    /// The copy receives a fresh ID, so edits/history cannot collide.
    pub fn duplicate_selected(&mut self) -> Result<()> {
        if self.layers.len() >= MAX_LAYERS {
            bail!("Maximum number of layers reached");
        }
        let id = self.next_layer_id;
        let next_id = self
            .next_layer_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Layer ID counter exhausted"))?;
        // Avoid allocating a full second copy of a multi-channel 4K layer
        // when it would exceed the same 512-MiB decoded document budget
        // enforced by the native .kalium importer.
        let used = self.decoded_bytes();
        let selected = self.selected_layer();
        let extra = (selected.mask.len() as u64).saturating_add(
            selected
                .pixels
                .values()
                .map(|image| image.len() as u64)
                .sum::<u64>(),
        );
        if used.saturating_add(extra) > 512 * 1024 * 1024 {
            bail!("Duplicating this layer would exceed the 512-MiB texture budget");
        }
        let mut copy = selected.clone();
        copy.id = id;
        copy.name = format!("{} copy", copy.name);
        self.next_layer_id = next_id;
        self.selected += 1;
        self.layers.insert(self.selected, copy);
        Ok(())
    }
    /// Inversion is an edit on a persistent grayscale layer mask.
    pub fn invert_selected_mask(&mut self) -> bool {
        let mask = &mut self.selected_layer_mut().mask;
        let dirty = !mask.is_empty();
        for pixel in mask.pixels_mut() {
            pixel.0[0] = 255 - pixel.0[0];
        }
        dirty
    }
    pub fn remove_layer(&mut self) -> Option<Layer> {
        if self.layers.len() > 1 {
            let removed = self.layers.remove(self.selected);
            self.selected = self.selected.min(self.layers.len() - 1);
            Some(removed)
        } else {
            None
        }
    }
    pub fn move_selected(&mut self, up: bool) {
        let dst = if up {
            self.selected
                .checked_add(1)
                .filter(|i| *i < self.layers.len())
        } else {
            self.selected.checked_sub(1)
        };
        if let Some(dst) = dst {
            self.layers.swap(self.selected, dst);
            self.selected = dst;
        }
    }
    pub fn try_resize(&mut self, size: u32) -> Result<()> {
        if !(64..=MAX_SIZE).contains(&size) {
            bail!("Texture resolution must be between 64 and {MAX_SIZE}");
        }
        if size == self.size {
            return Ok(());
        }

        let current_active_bytes = layer_storage_bytes(&self.layers);
        let inactive_bytes = self
            .decoded_bytes()
            .checked_sub(current_active_bytes)
            .ok_or_else(|| anyhow::anyhow!("Invalid decoded texture byte accounting"))?;
        ensure_resize_budget(
            inactive_bytes,
            projected_layer_storage_bytes(&self.layers, size)?,
            size,
        )?;

        for layer in &mut self.layers {
            layer.mask = image::imageops::resize(
                &layer.mask,
                size,
                size,
                image::imageops::FilterType::Nearest,
            );
            for pixels in layer.pixels.values_mut() {
                *pixels = image::imageops::resize(
                    pixels,
                    size,
                    size,
                    image::imageops::FilterType::Nearest,
                );
            }
        }
        self.size = size;
        Ok(())
    }
    pub fn composite(&self, channel: Channel) -> RgbaImage {
        composite_layers(&self.layers, self.size, channel)
    }
    pub fn validate(&self) -> Result<()> {
        self.mesh.validate()?;
        if self.material_bindings.len() != self.mesh.material_count() {
            bail!("Material binding count does not match imported mesh slots");
        }
        for id in self.material_bindings.iter().flatten() {
            if *id != self.active_set_id && !self.texture_sets.iter().any(|s| s.id == *id) {
                bail!("Material binding references missing texture set {id}");
            }
        }
        if self.texture_set_count() > MAX_TEXTURE_SETS {
            bail!("Too many texture sets");
        }
        if self.active_set_id == 0 || self.next_set_id <= self.active_set_id {
            bail!("Invalid active texture set ID");
        }
        validate_texture_set(
            self.size,
            &self.layers,
            self.selected,
            self.next_layer_id,
            &self.active_set_name,
        )?;
        let mut ids = HashSet::from([self.active_set_id]);
        for set in &self.texture_sets {
            if set.id == 0 || set.id >= self.next_set_id || !ids.insert(set.id) {
                bail!("Duplicate or invalid texture set ID");
            }
            validate_texture_set(
                set.size,
                &set.layers,
                set.selected,
                set.next_layer_id,
                &set.name,
            )?;
        }
        if self.decoded_bytes() > MAX_DECODED_BYTES {
            bail!("Project exceeds 512-MiB decoded texture budget");
        }
        Ok(())
    }
}

fn layer_storage_bytes(layers: &[Layer]) -> u64 {
    layers.iter().fold(0u64, |sum, layer| {
        sum.saturating_add(layer.mask.len() as u64).saturating_add(
            layer
                .pixels
                .values()
                .map(|image| image.len() as u64)
                .fold(0u64, u64::saturating_add),
        )
    })
}

fn projected_layer_storage_bytes(layers: &[Layer], size: u32) -> Result<u64> {
    let texels = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Resized texture dimensions overflow"))?;
    layers.iter().try_fold(0u64, |total, layer| {
        let layer_bytes = (texels)
            .checked_add(
                texels
                    .checked_mul(layer.pixels.len() as u64)
                    .and_then(|bytes| bytes.checked_mul(4))
                    .ok_or_else(|| anyhow::anyhow!("Resized texture byte count overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("Resized texture byte count overflow"))?;
        total
            .checked_add(layer_bytes)
            .ok_or_else(|| anyhow::anyhow!("Resized texture byte count overflow"))
    })
}

fn ensure_resize_budget(inactive_bytes: u64, active_bytes: u64, size: u32) -> Result<()> {
    let projected_bytes = inactive_bytes
        .checked_add(active_bytes)
        .ok_or_else(|| anyhow::anyhow!("Resized texture byte count overflow"))?;
    if projected_bytes > MAX_DECODED_BYTES {
        bail!("Resizing to {size} × {size} would exceed the 512-MiB document texture budget");
    }
    Ok(())
}

fn validate_texture_set(
    size: u32,
    layers: &[Layer],
    selected: usize,
    next_layer_id: u32,
    name: &str,
) -> Result<()> {
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid texture dimension");
    }
    if name.trim().is_empty() || name.len() > 128 {
        bail!("Invalid texture set name");
    }
    if layers.is_empty() || layers.len() > MAX_LAYERS {
        bail!("Invalid layer count");
    }
    if selected >= layers.len() {
        bail!("Invalid selected layer");
    }
    let mut ids = HashSet::new();
    if next_layer_id <= layers.iter().map(|l| l.id).max().unwrap_or(0) {
        bail!("Invalid next layer ID");
    }
    for layer in layers {
        if !ids.insert(layer.id) {
            bail!("Duplicate layer ID");
        }
        if layer.name.is_empty() || layer.name.len() > 1024 {
            bail!("Invalid layer name");
        }
        if layer.mask.dimensions() != (size, size) {
            bail!("Invalid layer mask size");
        }
        if !layer.opacity.is_finite() || !(0.0..=1.0).contains(&layer.opacity) {
            bail!("Invalid opacity");
        }
        for image in layer.pixels.values() {
            if image.dimensions() != (size, size) {
                bail!("Invalid layer texture size");
            }
        }
    }
    Ok(())
}

fn composite_layers(layers: &[Layer], size: u32, channel: Channel) -> RgbaImage {
    composite_layers_region(layers, size, channel, 0, 0, size, size)
}

fn composite_layers_region(
    layers: &[Layer],
    size: u32,
    channel: Channel,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> RgbaImage {
    let mut out = ImageBuffer::from_pixel(width, height, Rgba(channel.background()));
    for layer in layers {
        if !layer.visible || layer.opacity <= 0.0 {
            continue;
        }
        if layer.disabled_channels.contains(&channel) {
            continue;
        }
        let Some(src) = layer.pixels.get(&channel) else {
            continue;
        };
        let data = src.as_raw();
        let masks = layer.mask.as_raw();
        let dst = out.as_mut();
        for local_y in 0..height {
            let source_y = y + local_y;
            for local_x in 0..width {
                let source_x = x + local_x;
                let source_index = (source_y * size + source_x) as usize;
                let destination_index = (local_y * width + local_x) as usize;
                let s = &data[source_index * 4..source_index * 4 + 4];
                let dst_pixel = &mut dst[destination_index * 4..destination_index * 4 + 4];
                let a =
                    (s[3] as f32 / 255.0) * layer.opacity * (masks[source_index] as f32 / 255.0);
                if a <= 0.0 {
                    continue;
                }
                match channel {
                    Channel::BaseColor | Channel::Emissive => {
                        for c in 0..3 {
                            let source = srgb_to_linear(s[c] as f32 / 255.0);
                            let destination = srgb_to_linear(dst_pixel[c] as f32 / 255.0);
                            let blended = match layer.blend {
                                BlendMode::Normal => source,
                                BlendMode::Multiply => destination * source,
                                BlendMode::Add => (destination + source).min(1.0),
                                BlendMode::Detail => source,
                            };
                            let linear = destination + (blended - destination) * a;
                            dst_pixel[c] =
                                (linear_to_srgb(linear) * 255.0).round().clamp(0.0, 255.0) as u8;
                        }
                    }
                    Channel::Normal => {
                        let destination = decode_normal(&dst_pixel[..3]);
                        let source = decode_normal(&s[..3]);
                        let blended = if layer.blend == BlendMode::Detail {
                            // Detail maps are weighted against the neutral tangent-space
                            // normal before reorientation, so opacity/mask zero is identity.
                            let weighted_detail = normalize3([
                                source[0] * a,
                                source[1] * a,
                                1.0 + (source[2] - 1.0) * a,
                            ]);
                            reoriented_normal(destination, weighted_detail)
                        } else {
                            // Other layer modes retain weighted orientation interpolation.
                            normalize3([
                                destination[0] * (1.0 - a) + source[0] * a,
                                destination[1] * (1.0 - a) + source[1] * a,
                                destination[2] * (1.0 - a) + source[2] * a,
                            ])
                        };
                        for c in 0..3 {
                            dst_pixel[c] =
                                ((blended[c] * 0.5 + 0.5) * 255.0).round().clamp(0.0, 255.0) as u8;
                        }
                    }
                    _ => {
                        for c in 0..3 {
                            let source = s[c] as f32;
                            let destination = dst_pixel[c] as f32;
                            let blended = match layer.blend {
                                BlendMode::Normal => source,
                                BlendMode::Multiply => destination * source / 255.0,
                                BlendMode::Add => (destination + source).min(255.0),
                                BlendMode::Detail => source,
                            };
                            dst_pixel[c] = (destination + (blended - destination) * a)
                                .round()
                                .clamp(0.0, 255.0) as u8;
                        }
                    }
                }
            }
        }
    }
    out
}

fn srgb_to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(channel: f32) -> f32 {
    if channel <= 0.0031308 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

fn decode_normal(encoded: &[u8]) -> [f32; 3] {
    normalize3([
        encoded[0] as f32 / 127.5 - 1.0,
        encoded[1] as f32 / 127.5 - 1.0,
        encoded[2] as f32 / 127.5 - 1.0,
    ])
}

fn normalize3(vector: [f32; 3]) -> [f32; 3] {
    let length_squared = vector
        .iter()
        .map(|component| component * component)
        .sum::<f32>();
    if length_squared <= 1.0e-12 {
        [0.0, 0.0, 1.0]
    } else {
        let inverse_length = length_squared.sqrt().recip();
        vector.map(|component| component * inverse_length)
    }
}

/// Reoriented normal mapping (RNM), with `base` as the accumulated surface
/// orientation and `detail` in the tangent frame of the neutral surface.
fn reoriented_normal(base: [f32; 3], detail: [f32; 3]) -> [f32; 3] {
    let t = [base[0], base[1], base[2] + 1.0];
    let u = [-detail[0], -detail[1], detail[2]];
    let dot = t[0] * u[0] + t[1] * u[1] + t[2] * u[2];
    normalize3([
        t[0] * dot - u[0] * t[2],
        t[1] * dot - u[1] * t[2],
        t[2] * dot - u[2] * t[2],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn importing_mesh_pins_existing_set_for_all_materials() {
        let mut doc = Document::new(64);
        let mut mesh = Mesh::demo_cube();
        mesh.materials.push("Leather".into());
        mesh.triangle_materials[0] = 1;
        doc.import_mesh(mesh).unwrap();
        assert_eq!(doc.material_bindings, vec![Some(1), Some(1)]);
        doc.add_texture_set("Metal").unwrap();
        assert_eq!(doc.texture_set_for_material(0), 1);
        assert_eq!(doc.texture_set_for_material(1), 1);
    }
    #[test]
    fn material_bindings_survive_set_switch_and_removal() {
        let mut doc = Document::new(64);
        doc.mesh.materials.push("Leather".to_owned());
        doc.mesh.triangle_materials[0] = 1;
        doc.material_bindings.push(None);
        let metal = doc.add_texture_set("Metal").unwrap();
        doc.bind_material(0, Some(metal)).unwrap();
        doc.activate_texture_set(1).unwrap();
        assert_eq!(doc.texture_set_for_material(0), metal);
        assert_eq!(doc.texture_set_for_material(1), 1);
        doc.activate_texture_set(metal).unwrap();
        doc.remove_active_texture_set().unwrap();
        assert_eq!(doc.material_bindings[0], None);
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
        let doc = Document::new(64);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(10, 10).0,
            Channel::BaseColor.background()
        );
    }
    #[test]
    fn duplicate_preserves_channels_and_mask_but_creates_new_id() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(2, 3, Rgba([123, 123, 123, 255]));
        doc.selected_layer_mut().mask.put_pixel(2, 3, Luma([45]));
        doc.duplicate_selected().unwrap();
        assert_eq!(doc.layers.len(), 2);
        assert_eq!(doc.selected, 1);
        assert_ne!(doc.layers[0].id, doc.layers[1].id);
        assert_eq!(doc.layers[1].mask.get_pixel(2, 3).0, [45]);
        assert_eq!(
            doc.layers[1].pixels[&Channel::Metallic].get_pixel(2, 3).0,
            [123, 123, 123, 255]
        );
        doc.layers[1]
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(2, 3, Rgba([1, 1, 1, 255]));
        assert_eq!(
            doc.layers[0].pixels[&Channel::Metallic].get_pixel(2, 3).0,
            [123, 123, 123, 255]
        );
        doc.invert_selected_mask();
        assert_eq!(doc.layers[1].mask.get_pixel(2, 3).0, [210]);
        doc.layers[1].disabled_channels.insert(Channel::Metallic);
        assert!(!doc.layers[0].disabled_channels.contains(&Channel::Metallic));
        doc.validate().unwrap();
    }
    #[test]
    fn resize_rejects_over_budget_without_mutating_document() {
        let mut doc = Document::new(64);
        for channel in Channel::ALL {
            doc.selected_layer_mut().pixels_mut(channel, 64);
        }
        doc.selected_layer_mut().mask.put_pixel(3, 4, Luma([87]));
        let before_bytes = doc.decoded_bytes();
        let before_base = doc.composite(Channel::BaseColor).get_pixel(3, 4).0;

        let error = doc.try_resize(MAX_SIZE).unwrap_err();
        assert!(error.to_string().contains("512-MiB"));
        assert_eq!(doc.size, 64);
        assert_eq!(doc.decoded_bytes(), before_bytes);
        assert_eq!(doc.selected_layer().mask.get_pixel(3, 4).0, [87]);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(3, 4).0,
            before_base
        );
    }

    #[test]
    fn resize_active_set_preserves_inactive_set_resolution_and_pixels() {
        let mut doc = Document::new(64);
        let first_set = doc.active_set_id;
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(2, 3, Rgba([9, 18, 27, 255]));
        let second_set = doc.add_texture_set("Inactive").unwrap();
        doc.selected_layer_mut()
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(5, 6, Rgba([111, 111, 111, 255]));
        doc.activate_texture_set(first_set).unwrap();

        doc.try_resize(128).unwrap();
        assert_eq!(doc.size, 128);
        assert_eq!(doc.texture_sets[0].id, second_set);
        assert_eq!(doc.texture_sets[0].size, 64);
        assert_eq!(
            doc.texture_sets[0].layers[0].pixels[&Channel::Metallic]
                .get_pixel(5, 6)
                .0,
            [111, 111, 111, 255]
        );
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor]
                .get_pixel(4, 6)
                .0,
            [9, 18, 27, 255]
        );
        doc.validate().unwrap();
    }

    #[test]
    fn resize_budget_includes_inactive_texture_sets() {
        assert!(ensure_resize_budget(MAX_DECODED_BYTES - 64, 64, 256).is_ok());
        let error = ensure_resize_budget(MAX_DECODED_BYTES - 64, 65, 256).unwrap_err();
        assert!(error.to_string().contains("512-MiB"));
    }

    #[test]
    fn painted_layer_blends_with_background() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 4, Rgba([255, 0, 0, 255]));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 4).0,
            [255, 0, 0, 255]
        );
        doc.selected_layer_mut().mask.put_pixel(4, 4, Luma([0]));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 4).0,
            Channel::BaseColor.background()
        );
    }

    #[test]
    fn base_color_composites_in_linear_light() {
        let mut under = Layer::new(1, "Under", 64);
        under
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(0, 0, Rgba([0, 0, 255, 255]));
        let mut top = Layer::new(2, "Top", 64);
        top.pixels_mut(Channel::BaseColor, 64)
            .put_pixel(0, 0, Rgba([255, 0, 0, 128]));

        let result = composite_layers(&[under, top], 64, Channel::BaseColor);
        let pixel = result.get_pixel(0, 0).0;
        assert!((pixel[0] as i16 - 188).abs() <= 1);
        assert_eq!(pixel[1], 0);
        assert!((pixel[2] as i16 - 187).abs() <= 1);
    }

    #[test]
    fn emissive_composites_in_linear_light_with_layer_alpha_and_mask() {
        let mut under = Layer::new(1, "Under", 64);
        under
            .pixels_mut(Channel::Emissive, 64)
            .put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        let mut top = Layer::new(2, "Top", 64);
        top.opacity = 0.5;
        top.mask.put_pixel(0, 0, Luma([128]));
        top.pixels_mut(Channel::Emissive, 64)
            .put_pixel(0, 0, Rgba([255, 0, 0, 255]));

        let result = composite_layers(&[under, top], 64, Channel::Emissive);
        let pixel = result.get_pixel(0, 0).0;
        // The combined layer opacity and mask weight is approximately one quarter.
        // Linear-light red at 25% encodes to sRGB 137, unlike gamma-space 64.
        assert!((pixel[0] as i16 - 137).abs() <= 1);
        assert_eq!(pixel[1], 0);
        assert_eq!(pixel[2], 0);
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn emissive_add_blend_uses_linear_light() {
        let mut under = Layer::new(1, "Under", 64);
        under
            .pixels_mut(Channel::Emissive, 64)
            .put_pixel(0, 0, Rgba([128, 128, 128, 255]));
        let mut top = Layer::new(2, "Top", 64);
        top.blend = BlendMode::Add;
        top.pixels_mut(Channel::Emissive, 64)
            .put_pixel(0, 0, Rgba([128, 0, 0, 128]));

        let result = composite_layers(&[under, top], 64, Channel::Emissive);
        let pixel = result.get_pixel(0, 0).0;
        // Add is evaluated in linear light before the 50% source-alpha interpolation.
        assert!((pixel[0] as i16 - 154).abs() <= 1);
        assert_eq!(pixel[1], 128);
        assert_eq!(pixel[2], 128);
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn normal_compositing_interpolates_and_normalizes_vectors() {
        let mut under = Layer::new(1, "Under", 64);
        under
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(0, 0, Rgba([128, 128, 255, 255]));
        let mut top = Layer::new(2, "Top", 64);
        top.pixels_mut(Channel::Normal, 64)
            .put_pixel(0, 0, Rgba([218, 128, 218, 128]));
        // The layer blend mode is a color operation; normal channels always use
        // orientation interpolation with alpha, opacity and mask as its weight.
        top.blend = BlendMode::Multiply;

        let result = composite_layers(&[under, top], 64, Channel::Normal);
        let pixel = result.get_pixel(0, 0).0;
        assert!((pixel[0] as i16 - 176).abs() <= 2);
        assert!((pixel[1] as i16 - 128).abs() <= 1);
        assert!((pixel[2] as i16 - 245).abs() <= 2);
        let decoded = decode_normal(&pixel[..3]);
        let length = decoded
            .iter()
            .map(|component| component * component)
            .sum::<f32>();
        assert!((length - 1.0).abs() < 0.02);
    }

    #[test]
    fn detail_normal_compositing_uses_reoriented_normal_mapping() {
        let base = normalize3([0.45, -0.2, 0.87]);
        let detail = normalize3([-0.3, 0.5, 0.81]);
        let combined = reoriented_normal(base, detail);

        assert!(combined.iter().all(|component| component.is_finite()));
        assert!((combined.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1.0e-6);
        assert!(
            combined[0] < base[0],
            "detail X must reorient the base normal"
        );
        assert!(
            combined[1] > base[1],
            "detail Y must reorient the base normal"
        );
        assert_ne!(
            combined,
            normalize3([
                base[0] + detail[0],
                base[1] + detail[1],
                base[2] + detail[2],
            ])
        );
    }

    #[test]
    fn detail_normal_compositing_respects_alpha_layer_opacity_and_mask() {
        let mut under = Layer::new(1, "Under", 64);
        under
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(0, 0, Rgba([184, 103, 232, 255]));
        let mut top = Layer::new(2, "Detail", 64);
        top.blend = BlendMode::Detail;
        top.opacity = 0.5;
        top.mask.put_pixel(0, 0, Luma([128]));
        top.pixels_mut(Channel::Normal, 64)
            .put_pixel(0, 0, Rgba([188, 188, 224, 128]));

        let result = composite_layers(&[under.clone(), top.clone()], 64, Channel::Normal);
        let actual = decode_normal(&result.get_pixel(0, 0).0[..3]);
        let base = decode_normal(&under.pixels[&Channel::Normal].get_pixel(0, 0).0[..3]);
        let detail = decode_normal(&top.pixels[&Channel::Normal].get_pixel(0, 0).0[..3]);
        let weight = (128.0 / 255.0) * 0.5 * (128.0 / 255.0);
        let weighted_detail = normalize3([
            detail[0] * weight,
            detail[1] * weight,
            1.0 + (detail[2] - 1.0) * weight,
        ]);
        let expected = reoriented_normal(base, weighted_detail);
        for axis in 0..3 {
            assert!((actual[axis] - expected[axis]).abs() < 0.015);
        }

        top.mask.put_pixel(0, 0, Luma([0]));
        let transparent = composite_layers(&[under.clone(), top], 64, Channel::Normal);
        let neutral_result = decode_normal(&transparent.get_pixel(0, 0).0[..3]);
        for axis in 0..3 {
            assert!((neutral_result[axis] - base[axis]).abs() < 0.01);
        }
    }

    #[test]
    fn disabled_channel_keeps_source_but_skips_compositing() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 5, Rgba([24, 38, 52, 255]));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 5).0,
            [24, 38, 52, 255]
        );
        doc.selected_layer_mut()
            .disabled_channels
            .insert(Channel::BaseColor);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 5).0,
            Channel::BaseColor.background()
        );
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor]
                .get_pixel(4, 5)
                .0,
            [24, 38, 52, 255]
        );
        doc.selected_layer_mut()
            .disabled_channels
            .remove(&Channel::BaseColor);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 5).0,
            [24, 38, 52, 255]
        );
    }

    #[test]
    fn texture_sets_keep_independent_layers_and_resolutions() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(1, 1, Rgba([200, 0, 0, 255]));
        let second = doc.add_texture_set("Armor").unwrap();
        assert_eq!(doc.active_set_id, second);
        assert_eq!(doc.texture_set_count(), 2);
        doc.try_resize(128).unwrap();
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 128)
            .put_pixel(1, 1, Rgba([0, 200, 0, 255]));
        doc.activate_texture_set(1).unwrap();
        assert_eq!(doc.size, 64);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(1, 1).0,
            [200, 0, 0, 255]
        );
        assert_eq!(
            doc.composite_for_set(second, Channel::BaseColor)
                .unwrap()
                .get_pixel(1, 1)
                .0,
            [0, 200, 0, 255]
        );
        doc.activate_texture_set(second).unwrap();
        assert_eq!(doc.size, 128);
        doc.validate().unwrap();
        doc.remove_active_texture_set().unwrap();
        assert_eq!(doc.texture_set_count(), 1);
        assert_eq!(doc.active_set_id, 1);
    }

    #[test]
    fn composite_region_matches_full_composite_for_channels_and_layer_semantics() {
        fn fill_set(layers: &mut Vec<Layer>, size: u32) {
            while layers.len() < 5 {
                let id = layers.len() as u32 + 1;
                layers.push(Layer::new(id, format!("Layer {id}"), size));
            }
            for (layer_index, layer) in layers.iter_mut().enumerate() {
                layer.visible = layer_index != 4;
                layer.opacity = [1.0, 0.65, 0.9, 0.4][layer_index % 4];
                layer.blend = BlendMode::ALL[layer_index % 4];
                for y in 0..size {
                    for x in 0..size {
                        let mask = ((x * 17 + y * 29 + layer_index as u32 * 31) % 256) as u8;
                        layer.mask.put_pixel(x, y, Luma([mask]));
                    }
                }
                for channel in Channel::ALL {
                    let image = layer.pixels_mut(channel, size);
                    for y in 0..size {
                        for x in 0..size {
                            let seed = (x * 11 + y * 23 + layer_index as u32 * 47) as u8;
                            image.put_pixel(
                                x,
                                y,
                                Rgba([seed, seed.wrapping_add(53), seed.wrapping_add(117), 255]),
                            );
                        }
                    }
                }
                if layer_index == 1 {
                    layer.disabled_channels.insert(Channel::Metallic);
                }
            }
        }

        let mut doc = Document::new(64);
        fill_set(&mut doc.layers, doc.size);
        let inactive_id = doc.add_texture_set("Inactive").unwrap();
        fill_set(&mut doc.layers, doc.size);
        doc.activate_texture_set(1).unwrap();

        for set_id in [1, inactive_id] {
            for channel in Channel::ALL {
                let full = doc.composite_for_set(set_id, channel).unwrap();
                let expected = image::imageops::crop_imm(&full, 7, 13, 29, 23).to_image();
                let actual = doc
                    .composite_region_for_set(set_id, channel, 7, 13, 29, 23)
                    .unwrap();
                assert_eq!(actual.as_raw(), expected.as_raw(), "{set_id} {channel:?}");
            }
        }
    }

    #[test]
    fn composite_region_rejects_invalid_rectangles_and_missing_sets() {
        let doc = Document::new(64);
        assert!(doc
            .composite_region_for_set(999, Channel::BaseColor, 0, 0, 1, 1)
            .is_none());
        assert!(doc
            .composite_region_for_set(1, Channel::BaseColor, 0, 0, 0, 1)
            .is_none());
        assert!(doc
            .composite_region_for_set(1, Channel::BaseColor, 0, 0, 1, 0)
            .is_none());
        assert!(doc
            .composite_region_for_set(1, Channel::BaseColor, u32::MAX, 0, 1, 1)
            .is_none());
        assert!(doc
            .composite_region_for_set(1, Channel::BaseColor, 60, 0, 5, 1)
            .is_none());
        assert!(doc
            .composite_region_for_set(1, Channel::BaseColor, 0, 63, 1, 2)
            .is_none());
    }
}

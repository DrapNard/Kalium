use crate::layer_tree::{LayerNode, LayerTree, MAX_LAYER_TREE_NODES};
use crate::mesh::Mesh;
use crate::procedural::ProceduralGraph;
use anyhow::{bail, Result};
#[cfg(test)]
use image::Rgba;
use image::{GrayImage, ImageBuffer, Luma, RgbaImage};
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
    /// Optional non-destructive generated Roughness source.
    pub procedural_graph: Option<ProceduralGraph>,
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
            procedural_graph: None,
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
    pub layer_tree: LayerTree,
    /// Unknown manifest fields retained for forward-compatible project round trips.
    pub(crate) extensions: JsonMap<String, JsonValue>,
}

pub struct Document {
    /// Stable identity used to bind recovery journals to this project.
    pub project_id: Uuid,
    pub mesh: Mesh,
    pub size: u32,
    pub layer_tree: LayerTree,
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
        let layers = vec![Layer::new(1, "Paint Layer", size)];
        let layer_tree =
            LayerTree::from_flat(&layers, 0, 2).expect("the built-in one-layer tree is valid");
        Self {
            project_id: Uuid::new_v4(),
            mesh: Mesh::demo_cube(),
            size,
            layer_tree,
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

    #[cfg(test)]
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

    #[cfg(test)]
    pub fn import_mesh(&mut self, mesh: Mesh) -> Result<()> {
        self.replace_mesh(mesh).map(|_| ())
    }

    /// Replace the document mesh and return its former mesh and material bindings.
    /// Validation and binding allocation finish before either live field changes.
    pub fn replace_mesh(&mut self, mesh: Mesh) -> Result<(Mesh, Vec<Option<u32>>)> {
        mesh.validate()?;
        let mut bindings = Vec::new();
        bindings.try_reserve_exact(mesh.material_count())?;
        // Replacing a mesh never borrows old slot positions. Keep all newly
        // imported materials pinned to the current set until explicitly remapped.
        bindings.resize(mesh.material_count(), Some(self.active_set_id));
        let previous_mesh = std::mem::replace(&mut self.mesh, mesh);
        let previous_bindings = std::mem::replace(&mut self.material_bindings, bindings);
        Ok((previous_mesh, previous_bindings))
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

    /// Takes ownership of the active tree without cloning pixel buffers.
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
        let previous_tree = std::mem::replace(&mut self.layer_tree, target.layer_tree);
        let previous = TextureSet {
            id: self.active_set_id,
            name: std::mem::take(&mut self.active_set_name),
            size: self.size,
            layer_tree: previous_tree,
            extensions: std::mem::take(&mut self.active_set_extensions),
        };
        self.texture_sets.push(previous);
        self.active_set_id = target.id;
        self.active_set_name = target.name;
        self.size = target.size;
        self.active_set_extensions = target.extensions;
        Ok(true)
    }

    #[cfg(test)]
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
        let mask_bytes = u64::from(self.size)
            .checked_mul(u64::from(self.size))
            .ok_or_else(|| anyhow::anyhow!("Layer mask size overflow"))?;
        let projected = self
            .checked_decoded_bytes()?
            .checked_add(mask_bytes)
            .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
        if projected > MAX_DECODED_BYTES {
            bail!("Texture sets exceed 512-MiB decoded texture budget");
        }
        let new_set = TextureSet {
            id,
            name: name.to_owned(),
            size: self.size,
            layer_tree: LayerTree::from_flat(&[Layer::new(1, "Paint Layer", self.size)], 0, 2)?,
            extensions: JsonMap::new(),
        };
        self.texture_sets.push(new_set);
        self.next_set_id = next;
        self.activate_texture_set(id)?;
        Ok(id)
    }

    #[cfg(test)]
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

    pub fn checked_decoded_bytes(&self) -> Result<u64> {
        let mut total = self.layer_tree.checked_decoded_bytes()?;
        for set in &self.texture_sets {
            total = total
                .checked_add(set.layer_tree.checked_decoded_bytes()?)
                .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
        }
        Ok(total)
    }

    pub fn decoded_bytes(&self) -> u64 {
        self.checked_decoded_bytes().unwrap_or(u64::MAX)
    }

    /// Ensure a stroke can lazily create the selected Paint layer's channel
    /// without exceeding the document's decoded texture budget.
    pub fn ensure_selected_channel_capacity(&self, channel: Channel) -> Result<()> {
        let layer = self
            .selected_paint_layer()
            .ok_or_else(|| anyhow::anyhow!("Selected node is not a Paint layer"))?;
        if layer.pixels.contains_key(&channel) {
            return Ok(());
        }
        let channel_bytes = rgba_channel_bytes(self.size)
            .ok_or_else(|| anyhow::anyhow!("Texture channel size overflow"))?;
        let used_bytes = self.checked_decoded_bytes()?;
        if !decoded_allocation_fits_budget(used_bytes, channel_bytes) {
            bail!("Painting this channel would exceed the 512-MiB texture budget");
        }
        Ok(())
    }

    pub fn composite_for_set(&self, id: u32, channel: Channel) -> Option<RgbaImage> {
        if id == self.active_set_id {
            return Some(self.composite(channel));
        }
        self.texture_sets
            .iter()
            .find(|set| set.id == id)
            .map(|set| set.layer_tree.composite(set.size, channel))
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
        let (tree, size) = if set_id == self.active_set_id {
            (&self.layer_tree, self.size)
        } else {
            let set = self.texture_sets.iter().find(|set| set.id == set_id)?;
            (&set.layer_tree, set.size)
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

        Some(tree.composite_region(size, channel, x, y, width, height))
    }

    pub fn selected_layer(&self) -> Option<&Layer> {
        self.layer_tree.selected_paint_layer()
    }
    #[cfg(test)]
    pub fn selected_layer_mut(&mut self) -> Option<&mut Layer> {
        self.layer_tree.selected_paint_layer_mut()
    }
    pub fn selected_paint_layer(&self) -> Option<&Layer> {
        self.layer_tree.selected_paint_layer()
    }
    pub fn selected_paint_layer_mut(&mut self) -> Option<&mut Layer> {
        self.layer_tree.selected_paint_layer_mut()
    }
    pub fn paint_layer(&self, id: u32) -> Option<&Layer> {
        self.layer_tree.paint_layer(id)
    }
    pub fn paint_layer_mut(&mut self, id: u32) -> Option<&mut Layer> {
        self.layer_tree.paint_layer_mut(id)
    }
    pub fn select_node(&mut self, id: u32) -> Result<()> {
        self.layer_tree.select(id)
    }
    pub fn add_layer(&mut self) -> Result<()> {
        if self.layer_tree.ordered_paint_leaf_ids().len() >= MAX_LAYERS {
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
        let id = self.layer_tree.next_node_id;
        id.checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Layer ID counter exhausted"))?;
        let layer = Layer::new(id, format!("Paint Layer {}", id), self.size);
        let selected_id = self.layer_tree.selected_node_id;
        let parent = self.layer_tree.parent_id(selected_id).flatten();
        let path = self
            .layer_tree
            .path_to(selected_id)
            .ok_or_else(|| anyhow::anyhow!("Selected node is missing"))?;
        self.layer_tree.insert(
            parent,
            path.last().copied().unwrap_or(0) + 1,
            LayerNode::Paint(layer),
        )?;
        self.layer_tree.select(id)?;
        Ok(())
    }
    /// Duplicate the selected layer with all of its PBR channels and its mask.
    /// The copy receives a fresh ID, so edits/history cannot collide.
    #[cfg(test)]
    pub fn duplicate_selected(&mut self) -> Result<()> {
        if self.layer_tree.ordered_paint_leaf_ids().len() >= MAX_LAYERS {
            bail!("Maximum number of layers reached");
        }
        let id = self.layer_tree.next_node_id;
        id.checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Layer ID counter exhausted"))?;
        // Avoid allocating a full second copy of a multi-channel 4K layer
        // when it would exceed the same 512-MiB decoded document budget
        // enforced by the native .kalium importer.
        let used = self.decoded_bytes();
        let selected = self
            .selected_layer()
            .ok_or_else(|| anyhow::anyhow!("Selected node is not a Paint layer"))?;
        let extra = (selected.mask.len() as u64)
            .checked_add(selected.pixels.values().try_fold(0u64, |sum, image| {
                sum.checked_add(image.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))
            })?)
            .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
        if used
            .checked_add(extra)
            .is_none_or(|bytes| bytes > MAX_DECODED_BYTES)
        {
            bail!("Duplicating this layer would exceed the 512-MiB texture budget");
        }
        let mut copy = selected.clone();
        copy.id = id;
        copy.name = format!("{} copy", copy.name);
        let selected_id = self.layer_tree.selected_node_id;
        let parent = self.layer_tree.parent_id(selected_id).flatten();
        let path = self
            .layer_tree
            .path_to(selected_id)
            .ok_or_else(|| anyhow::anyhow!("Selected node is missing"))?;
        self.layer_tree.insert(
            parent,
            path.last().copied().unwrap_or(0) + 1,
            LayerNode::Paint(copy),
        )?;
        self.layer_tree.select(id)?;
        Ok(())
    }
    /// Inversion is an edit on a persistent grayscale layer mask.
    #[cfg(test)]
    pub fn invert_selected_mask(&mut self) -> bool {
        let Some(layer) = self.selected_layer_mut() else {
            return false;
        };
        let mask = &mut layer.mask;
        let dirty = !mask.is_empty();
        for pixel in mask.pixels_mut() {
            pixel.0[0] = 255 - pixel.0[0];
        }
        dirty
    }
    pub fn remove_layer(&mut self) -> Option<Layer> {
        let selected_id = self.layer_tree.selected_node_id;
        if !matches!(
            self.layer_tree.find_node(selected_id),
            Some(LayerNode::Paint(_))
        ) {
            return None;
        }
        let siblings = self
            .layer_tree
            .parent_id(selected_id)
            .and_then(|parent| self.layer_tree.siblings(parent).ok())?;
        if siblings.len() <= 1
            && self
                .layer_tree
                .parent_id(selected_id)
                .is_some_and(|parent| parent.is_none())
        {
            return None;
        }
        let selected_after = siblings
            .iter()
            .find(|node| node.id() != selected_id)
            .map(LayerNode::id)
            .or_else(|| self.layer_tree.parent_id(selected_id).flatten())?;
        let removed = self.layer_tree.remove(selected_id, selected_after).ok()?;
        match removed {
            LayerNode::Paint(layer) => Some(layer),
            _ => None,
        }
    }
    #[cfg(test)]
    pub fn move_selected(&mut self, up: bool) {
        let id = self.layer_tree.selected_node_id;
        let Some(parent) = self.layer_tree.parent_id(id) else {
            return;
        };
        let Ok(siblings) = self.layer_tree.siblings(parent) else {
            return;
        };
        let Some(index) = siblings.iter().position(|node| node.id() == id) else {
            return;
        };
        let dst = if up {
            index + 1
        } else {
            index.saturating_sub(1)
        };
        if (up && dst >= siblings.len()) || (!up && index == 0) {
            return;
        }
        let _ = self.layer_tree.move_node(id, parent, dst);
    }
    pub fn try_resize(&mut self, size: u32) -> Result<()> {
        if !(64..=MAX_SIZE).contains(&size) {
            bail!("Texture resolution must be between 64 and {MAX_SIZE}");
        }
        if size == self.size {
            return Ok(());
        }

        let current_active_bytes = self.layer_tree.checked_decoded_bytes()?;
        let inactive_bytes = self
            .decoded_bytes()
            .checked_sub(current_active_bytes)
            .ok_or_else(|| anyhow::anyhow!("Invalid decoded texture byte accounting"))?;
        ensure_tree_resize_peak_budget(&self.layer_tree, inactive_bytes, size)?;
        resize_tree_images(&mut self.layer_tree, size)?;
        self.size = size;
        Ok(())
    }
    pub fn composite(&self, channel: Channel) -> RgbaImage {
        self.layer_tree.composite(self.size, channel)
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
        validate_texture_set(self.size, &self.layer_tree, &self.active_set_name)?;
        let mut ids = HashSet::from([self.active_set_id]);
        for set in &self.texture_sets {
            if set.id == 0 || set.id >= self.next_set_id || !ids.insert(set.id) {
                bail!("Duplicate or invalid texture set ID");
            }
            validate_texture_set(set.size, &set.layer_tree, &set.name)?;
        }
        if self.checked_decoded_bytes()? > MAX_DECODED_BYTES {
            bail!("Project exceeds 512-MiB decoded texture budget");
        }
        Ok(())
    }
}

fn rgba_channel_bytes(size: u32) -> Option<u64> {
    u64::from(size).checked_mul(u64::from(size))?.checked_mul(4)
}

fn decoded_allocation_fits_budget(used_bytes: u64, additional_bytes: u64) -> bool {
    used_bytes
        .checked_add(additional_bytes)
        .is_some_and(|total| total <= MAX_DECODED_BYTES)
}

/// Bounds temporary resize allocations for all Paint, Fill, and Group masks
/// and Paint channels in one Texture Set.
fn ensure_tree_resize_peak_budget(tree: &LayerTree, inactive_bytes: u64, size: u32) -> Result<()> {
    let texels = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Resized texture dimensions overflow"))?;
    let mut images = Vec::new();
    let mut stack: Vec<&LayerNode> = tree.roots.iter().collect();
    while let Some(node) = stack.pop() {
        let mask = match node {
            LayerNode::Paint(layer) => &layer.mask,
            LayerNode::Fill(fill) => &fill.properties.mask,
            LayerNode::Group(group) => &group.properties.mask,
        };
        images.try_reserve(1)?;
        images.push((mask.len() as u64, texels));
        match node {
            LayerNode::Paint(layer) => {
                images.try_reserve(layer.pixels.len())?;
                images.extend(
                    layer
                        .pixels
                        .values()
                        .map(|image| (image.len() as u64, texels.saturating_mul(4))),
                );
            }
            LayerNode::Group(group) => {
                stack.try_reserve(group.children.len())?;
                stack.extend(group.children.iter());
            }
            LayerNode::Fill(_) => {}
        }
    }
    ensure_resize_peak_budget_for_images(inactive_bytes, &images, size, MAX_DECODED_BYTES)
}

fn resize_tree_images(tree: &mut LayerTree, size: u32) -> Result<()> {
    let mut stack: Vec<&mut LayerNode> = Vec::new();
    stack.try_reserve(MAX_LAYER_TREE_NODES)?;
    stack.extend(tree.roots.iter_mut());
    while let Some(node) = stack.pop() {
        match node {
            LayerNode::Paint(layer) => {
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
            LayerNode::Fill(fill) => {
                fill.properties.mask = image::imageops::resize(
                    &fill.properties.mask,
                    size,
                    size,
                    image::imageops::FilterType::Nearest,
                );
            }
            LayerNode::Group(group) => {
                group.properties.mask = image::imageops::resize(
                    &group.properties.mask,
                    size,
                    size,
                    image::imageops::FilterType::Nearest,
                );
                stack.extend(group.children.iter_mut());
            }
        }
    }
    Ok(())
}

fn ensure_resize_peak_budget_for_images(
    inactive_bytes: u64,
    images: &[(u64, u64)],
    size: u32,
    budget: u64,
) -> Result<()> {
    let mut remaining_sources = images.iter().try_fold(0u64, |sum, (source, _)| {
        sum.checked_add(*source)
            .ok_or_else(|| anyhow::anyhow!("Resize peak byte count overflow"))
    })?;
    let mut replaced_outputs = 0u64;
    for (source_bytes, destination_bytes) in images {
        let peak = inactive_bytes
            .checked_add(replaced_outputs)
            .and_then(|bytes| bytes.checked_add(remaining_sources))
            .and_then(|bytes| bytes.checked_add(*destination_bytes))
            .ok_or_else(|| anyhow::anyhow!("Resize peak byte count overflow"))?;
        if peak > budget {
            bail!("Resizing to {size} × {size} would exceed the 512-MiB document texture budget during allocation");
        }
        remaining_sources = remaining_sources
            .checked_sub(*source_bytes)
            .ok_or_else(|| anyhow::anyhow!("Invalid resize source byte accounting"))?;
        replaced_outputs = replaced_outputs
            .checked_add(*destination_bytes)
            .ok_or_else(|| anyhow::anyhow!("Resize peak byte count overflow"))?;
    }
    Ok(())
}

fn validate_texture_set(size: u32, tree: &LayerTree, name: &str) -> Result<()> {
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid texture dimension");
    }
    if name.trim().is_empty() || name.len() > 128 {
        bail!("Invalid texture set name");
    }
    tree.validate(size)?;
    if tree.ordered_paint_leaf_ids().len() > MAX_LAYERS {
        bail!("Invalid Paint layer count");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn composite_layers(layers: &[Layer], size: u32, channel: Channel) -> RgbaImage {
    composite_layers_region(layers, size, channel, 0, 0, size, size)
}

#[cfg(test)]
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
        let source_image = layer.pixels.get(&channel);
        let procedural_graph = if channel == Channel::Roughness {
            layer
                .procedural_graph
                .as_ref()
                .filter(|graph| graph.validate().is_ok())
        } else {
            None
        };
        if source_image.is_none() && procedural_graph.is_none() {
            continue;
        }
        let data = source_image.map(RgbaImage::as_raw);
        let occlusion_data = procedural_graph
            .and_then(|graph| graph.occlusion_source_layer_id())
            .and_then(|source_layer_id| layers.iter().find(|source| source.id == source_layer_id))
            .and_then(|source| source.pixels.get(&Channel::Occlusion))
            .map(|image| image.as_raw().as_slice());
        let masks = layer.mask.as_raw();
        let dst = out.as_mut();
        for local_y in 0..height {
            let source_y = y + local_y;
            let generated_row = procedural_graph.and_then(|graph| {
                graph
                    .render_roughness_span(size, source_y, x, width, occlusion_data)
                    .ok()
            });
            if data.is_none() && generated_row.is_none() {
                continue;
            }
            for local_x in 0..width {
                let source_x = x + local_x;
                let source_index = (source_y * size + source_x) as usize;
                let destination_index = (local_y * width + local_x) as usize;
                let composited_source: [u8; 4];
                let s = if let Some(data) = data {
                    let image_pixel = &data[source_index * 4..source_index * 4 + 4];
                    if let Some(row) = generated_row.as_ref() {
                        let generated = row[local_x as usize * 4];
                        let coverage = image_pixel[3] as f32 / 255.0;
                        let blended = (generated as f32
                            + (image_pixel[0] as f32 - generated as f32) * coverage)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                        // Authored Roughness pixels overlay the procedural base
                        // using their alpha; layer opacity/mask/blend then apply once.
                        composited_source = [blended, blended, blended, 255];
                        &composited_source[..]
                    } else {
                        image_pixel
                    }
                } else {
                    let row = generated_row.as_ref().expect("generated row was checked");
                    let row_index = local_x as usize * 4;
                    &row[row_index..row_index + 4]
                };
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

pub(crate) fn srgb_to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn linear_to_srgb(channel: f32) -> f32 {
    if channel <= 0.0031308 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

pub(crate) fn decode_normal(encoded: &[u8]) -> [f32; 3] {
    normalize3([
        encoded[0] as f32 / 127.5 - 1.0,
        encoded[1] as f32 / 127.5 - 1.0,
        encoded[2] as f32 / 127.5 - 1.0,
    ])
}

pub(crate) fn normalize3(vector: [f32; 3]) -> [f32; 3] {
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
pub(crate) fn reoriented_normal(base: [f32; 3], detail: [f32; 3]) -> [f32; 3] {
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
    fn channel_allocation_budget_accepts_exact_limit_and_rejects_overflow() {
        assert!(decoded_allocation_fits_budget(MAX_DECODED_BYTES - 64, 64));
        assert!(!decoded_allocation_fits_budget(MAX_DECODED_BYTES - 63, 64));
        assert!(!decoded_allocation_fits_budget(u64::MAX, 1));
        assert_eq!(rgba_channel_bytes(4096), Some(64 * 1024 * 1024));
    }

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
    fn replacing_mesh_returns_previous_state_and_rejects_invalid_input_atomically() {
        let mut doc = Document::new(64);
        doc.bind_material(0, None).unwrap();
        let previous_name = doc.mesh.name.clone();
        let previous_bindings = doc.material_bindings.clone();
        let mut replacement = Mesh::demo_cube();
        replacement.name = "Replacement".into();

        let (old_mesh, old_bindings) = doc.replace_mesh(replacement).unwrap();
        assert_eq!(old_mesh.name, previous_name);
        assert_eq!(old_bindings, previous_bindings);
        assert_eq!(doc.mesh.name, "Replacement");
        assert_eq!(doc.material_bindings, vec![Some(doc.active_set_id)]);

        let current_name = doc.mesh.name.clone();
        let current_bindings = doc.material_bindings.clone();
        let mut invalid = Mesh::demo_cube();
        invalid.indices[0] = u32::MAX;
        assert!(doc.replace_mesh(invalid).is_err());
        assert_eq!(doc.mesh.name, current_name);
        assert_eq!(doc.material_bindings, current_bindings);
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
            .unwrap()
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(2, 3, Rgba([123, 123, 123, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(2, 3, Luma([45]));
        doc.duplicate_selected().unwrap();
        let leaf_ids = doc.layer_tree.ordered_paint_leaf_ids();
        assert_eq!(leaf_ids.len(), 2);
        assert_eq!(doc.layer_tree.selected_node_id, leaf_ids[1]);
        assert_ne!(leaf_ids[0], leaf_ids[1]);
        assert_eq!(
            doc.paint_layer(leaf_ids[1]).unwrap().mask.get_pixel(2, 3).0,
            [45]
        );
        assert_eq!(
            doc.paint_layer(leaf_ids[1]).unwrap().pixels[&Channel::Metallic]
                .get_pixel(2, 3)
                .0,
            [123, 123, 123, 255]
        );
        doc.paint_layer_mut(leaf_ids[1])
            .unwrap()
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(2, 3, Rgba([1, 1, 1, 255]));
        assert_eq!(
            doc.paint_layer(leaf_ids[0]).unwrap().pixels[&Channel::Metallic]
                .get_pixel(2, 3)
                .0,
            [123, 123, 123, 255]
        );
        doc.invert_selected_mask();
        assert_eq!(
            doc.paint_layer(leaf_ids[1]).unwrap().mask.get_pixel(2, 3).0,
            [210]
        );
        doc.paint_layer_mut(leaf_ids[1])
            .unwrap()
            .disabled_channels
            .insert(Channel::Metallic);
        assert!(!doc
            .paint_layer(leaf_ids[0])
            .unwrap()
            .disabled_channels
            .contains(&Channel::Metallic));
        doc.validate().unwrap();
    }
    #[test]
    fn resize_peak_preflight_accepts_exact_limit_and_rejects_one_byte_less() {
        // The first image's source remains allocated while its four-byte
        // destination is created: 4 inactive + 16 source + 4 destination.
        let images = [(8, 4), (8, 4)];
        ensure_resize_peak_budget_for_images(4, &images, 2, 24).unwrap();
        let error = ensure_resize_peak_budget_for_images(4, &images, 2, 23).unwrap_err();
        assert!(error.to_string().contains("512-MiB"));
    }

    #[test]
    fn resize_peak_preflight_counts_source_and_destination_during_downsize() {
        // Even though the final image storage is only 12 bytes, the first
        // replacement briefly needs 4 inactive + 32 source + 4 destination.
        let images = [(16, 4), (16, 4)];
        ensure_resize_peak_budget_for_images(4, &images, 2, 40).unwrap();
        assert!(ensure_resize_peak_budget_for_images(4, &images, 2, 39).is_err());
    }

    #[test]
    fn resize_rejects_over_budget_without_mutating_document() {
        let mut doc = Document::new(64);
        for channel in Channel::ALL {
            doc.selected_layer_mut().unwrap().pixels_mut(channel, 64);
        }
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(3, 4, Luma([87]));
        let before_bytes = doc.decoded_bytes();
        let before_base = doc.composite(Channel::BaseColor).get_pixel(3, 4).0;

        let error = doc.try_resize(MAX_SIZE).unwrap_err();
        assert!(error.to_string().contains("512-MiB"));
        assert_eq!(doc.size, 64);
        assert_eq!(doc.decoded_bytes(), before_bytes);
        assert_eq!(doc.selected_layer().unwrap().mask.get_pixel(3, 4).0, [87]);
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
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(2, 3, Rgba([9, 18, 27, 255]));
        let second_set = doc.add_texture_set("Inactive").unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(5, 6, Rgba([111, 111, 111, 255]));
        doc.activate_texture_set(first_set).unwrap();

        doc.try_resize(128).unwrap();
        assert_eq!(doc.size, 128);
        assert_eq!(doc.texture_sets[0].id, second_set);
        assert_eq!(doc.texture_sets[0].size, 64);
        assert_eq!(
            doc.texture_sets[0]
                .layer_tree
                .paint_layer(1)
                .unwrap()
                .pixels[&Channel::Metallic]
                .get_pixel(5, 6)
                .0,
            [111, 111, 111, 255]
        );
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
                .get_pixel(4, 6)
                .0,
            [9, 18, 27, 255]
        );
        doc.validate().unwrap();
    }

    #[test]
    fn resize_peak_budget_includes_inactive_texture_sets() {
        let images = [(8, 4)];
        ensure_resize_peak_budget_for_images(MAX_DECODED_BYTES - 12, &images, 2, MAX_DECODED_BYTES)
            .unwrap();
        let error = ensure_resize_peak_budget_for_images(
            MAX_DECODED_BYTES - 11,
            &images,
            2,
            MAX_DECODED_BYTES,
        )
        .unwrap_err();
        assert!(error.to_string().contains("512-MiB"));
    }

    #[test]
    fn painted_layer_blends_with_background() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 4, Rgba([255, 0, 0, 255]));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 4).0,
            [255, 0, 0, 255]
        );
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(4, 4, Luma([0]));
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
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 5, Rgba([24, 38, 52, 255]));
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 5).0,
            [24, 38, 52, 255]
        );
        doc.selected_layer_mut()
            .unwrap()
            .disabled_channels
            .insert(Channel::BaseColor);
        assert_eq!(
            doc.composite(Channel::BaseColor).get_pixel(4, 5).0,
            Channel::BaseColor.background()
        );
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
                .get_pixel(4, 5)
                .0,
            [24, 38, 52, 255]
        );
        doc.selected_layer_mut()
            .unwrap()
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
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(1, 1, Rgba([200, 0, 0, 255]));
        let second = doc.add_texture_set("Armor").unwrap();
        assert_eq!(doc.active_set_id, second);
        assert_eq!(doc.texture_set_count(), 2);
        doc.try_resize(128).unwrap();
        doc.selected_layer_mut()
            .unwrap()
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
        fn fill_set(tree: &mut LayerTree, size: u32) {
            while tree.ordered_paint_leaf_ids().len() < 5 {
                let id = tree.next_node_id;
                tree.insert(
                    None,
                    tree.roots.len(),
                    LayerNode::Paint(Layer::new(id, format!("Layer {id}"), size)),
                )
                .unwrap();
            }
            for (layer_index, id) in tree.ordered_paint_leaf_ids().into_iter().enumerate() {
                let layer = tree.paint_layer_mut(id).unwrap();
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
        fill_set(&mut doc.layer_tree, doc.size);
        let inactive_id = doc.add_texture_set("Inactive").unwrap();
        fill_set(&mut doc.layer_tree, doc.size);
        doc.activate_texture_set(1).unwrap();

        for set_id in [1, inactive_id] {
            let tree = if set_id == doc.active_set_id {
                &doc.layer_tree
            } else {
                &doc.texture_sets
                    .iter()
                    .find(|set| set.id == set_id)
                    .unwrap()
                    .layer_tree
            };
            let set_size = if set_id == doc.active_set_id {
                doc.size
            } else {
                doc.texture_sets
                    .iter()
                    .find(|set| set.id == set_id)
                    .unwrap()
                    .size
            };
            let legacy_layers = tree
                .roots
                .iter()
                .map(|node| match node {
                    LayerNode::Paint(layer) => Ok(layer.clone()),
                    _ => bail!("fixture should remain a flat Paint tree"),
                })
                .collect::<Result<Vec<_>>>()
                .unwrap();
            for channel in Channel::ALL {
                let full = doc.composite_for_set(set_id, channel).unwrap();
                let legacy = composite_layers(&legacy_layers, set_size, channel);
                if full != legacy {
                    let mismatch = full
                        .pixels()
                        .zip(legacy.pixels())
                        .position(|(actual, expected)| actual != expected)
                        .unwrap();
                    let x = mismatch as u32 % set_size;
                    let y = mismatch as u32 / set_size;
                    panic!(
                        "legacy flat parity for {set_id} {channel:?} at ({x}, {y}): {:?} != {:?}",
                        full.get_pixel(x, y),
                        legacy.get_pixel(x, y)
                    );
                }
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

    #[test]
    fn procedural_roughness_composites_full_and_bounded_regions_identically() {
        let mut doc = Document::new(64);
        let graph = ProceduralGraph::value_noise(23, 5.0);
        doc.selected_layer_mut().unwrap().procedural_graph = Some(graph.clone());

        let expected = graph.render_roughness(doc.size).unwrap();
        let actual = doc.composite(Channel::Roughness);
        assert_eq!(actual, expected);

        let region = doc
            .composite_region_for_set(1, Channel::Roughness, 11, 17, 19, 13)
            .unwrap();
        for y in 0..13 {
            for x in 0..19 {
                assert_eq!(region.get_pixel(x, y), expected.get_pixel(x + 11, y + 17));
            }
        }
    }

    #[test]
    fn procedural_roughness_respects_layer_mask_opacity_and_channel_switch() {
        let mut doc = Document::new(64);
        let layer = doc.selected_layer_mut().unwrap();
        layer.procedural_graph = Some(ProceduralGraph::value_noise(3, 2.0));
        layer.opacity = 0.5;
        layer.mask.put_pixel(0, 0, Luma([0]));

        let graph = layer.procedural_graph.as_ref().unwrap();
        let row = graph.render_roughness_row(64, 0).unwrap();
        let composite = doc.composite(Channel::Roughness);
        assert_eq!(composite.get_pixel(0, 0).0[0], 170);
        let expected = (170.0_f32 + (row[4] as f32 - 170.0) * 0.5).round() as u8;
        assert_eq!(composite.get_pixel(1, 0).0[0], expected);

        doc.selected_layer_mut()
            .unwrap()
            .disabled_channels
            .insert(Channel::Roughness);
        assert!(doc
            .composite(Channel::Roughness)
            .pixels()
            .all(|pixel| pixel.0 == Channel::Roughness.background()));
    }

    #[test]
    fn ao_driven_roughness_samples_stable_source_layer_and_matches_region_composite() {
        let mut doc = Document::new(64);
        let mut ao = Layer::new(2, "Baked AO", 64);
        let mut image = RgbaImage::from_pixel(64, 64, image::Rgba([255, 255, 255, 255]));
        image.put_pixel(13, 17, image::Rgba([0, 0, 0, 255]));
        image.put_pixel(14, 17, image::Rgba([128, 128, 128, 255]));
        ao.pixels.insert(Channel::Occlusion, image);
        doc.layer_tree
            .insert(None, doc.layer_tree.roots.len(), LayerNode::Paint(ao))
            .unwrap();
        let mut roughness = Layer::new(3, "AO-driven Roughness", 64);
        let graph = ProceduralGraph::value_noise_times_occlusion(41, 7.0, 2);
        roughness.procedural_graph = Some(graph.clone());
        doc.layer_tree
            .insert(
                None,
                doc.layer_tree.roots.len(),
                LayerNode::Paint(roughness),
            )
            .unwrap();

        let full = doc.composite(Channel::Roughness);
        let noise = ProceduralGraph::value_noise(41, 7.0);
        assert_eq!(full.get_pixel(13, 17).0[0], 0);
        let expected = (noise
            .evaluate_roughness([14.5 / 64.0, 17.5 / 64.0])
            .unwrap()
            * (128.0 / 255.0)
            * 255.0)
            .round() as u8;
        assert_eq!(full.get_pixel(14, 17).0[0], expected);

        let region = doc
            .composite_region_for_set(1, Channel::Roughness, 11, 15, 9, 5)
            .unwrap();
        for y in 0..5 {
            for x in 0..9 {
                assert_eq!(region.get_pixel(x, y), full.get_pixel(x + 11, y + 15));
            }
        }
        assert_eq!(full, doc.composite(Channel::Roughness));
        graph.validate().unwrap();
    }

    #[test]
    fn procedural_roughness_remains_active_under_authored_alpha_overlay() {
        let mut doc = Document::new(64);
        let mut roughness = RgbaImage::from_pixel(64, 64, image::Rgba([10, 10, 10, 0]));
        roughness.put_pixel(8, 9, image::Rgba([20, 20, 20, 128]));
        doc.selected_layer_mut()
            .unwrap()
            .pixels
            .insert(Channel::Roughness, roughness);
        doc.selected_layer_mut().unwrap().procedural_graph =
            Some(ProceduralGraph::value_noise(91, 5.0));

        let generated = ProceduralGraph::value_noise(91, 5.0)
            .render_roughness(64)
            .unwrap();
        let composite = doc.composite(Channel::Roughness);
        assert_eq!(composite.get_pixel(7, 9), generated.get_pixel(7, 9));
        let base = generated.get_pixel(8, 9).0[0] as f32;
        let expected = (base + (20.0 - base) * (128.0 / 255.0)).round() as u8;
        assert_eq!(composite.get_pixel(8, 9).0[0], expected);

        let region = doc
            .composite_region_for_set(1, Channel::Roughness, 5, 7, 8, 4)
            .unwrap();
        for y in 0..4 {
            for x in 0..8 {
                assert_eq!(region.get_pixel(x, y), composite.get_pixel(x + 5, y + 7));
            }
        }
    }

    #[test]
    fn missing_ao_graph_source_falls_back_to_noise_without_retargeting() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut().unwrap().procedural_graph =
            Some(ProceduralGraph::value_noise_times_occlusion(11, 3.0, 999));
        let expected = ProceduralGraph::value_noise(11, 3.0)
            .render_roughness(64)
            .unwrap();
        assert_eq!(doc.composite(Channel::Roughness), expected);
    }
}

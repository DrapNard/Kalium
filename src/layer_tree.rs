//! Bounded, per-texture-set layer tree primitives.
//!
//! The legacy project model stores paint layers in a flat vector. This module
//! provides the structural core for migrating that vector to ordered groups
//! and value-based fill layers without changing the legacy layer payload.

use crate::document::{
    decode_normal, linear_to_srgb, srgb_to_linear, BlendMode, Channel, Layer, MAX_DECODED_BYTES,
    MAX_SIZE,
};
use anyhow::{bail, Result};
use image::{GrayImage, ImageBuffer, Rgba, RgbaImage};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::{HashMap, HashSet};

pub const MAX_LAYER_TREE_DEPTH: usize = 32;
pub const MAX_LAYER_TREE_NODES: usize = 4096;
pub const MAX_LAYER_TREE_EXTENSION_BYTES: usize = 48 * 1024 * 1024;

/// The common editable properties for nodes that do not use the legacy
/// pixel-backed [`Layer`] representation.
#[derive(Clone)]
pub struct LayerNodeProperties {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub blend: BlendMode,
    pub disabled_channels: HashSet<Channel>,
    pub mask: GrayImage,
    /// Unknown node fields retained for forward-compatible project round trips.
    pub extensions: JsonMap<String, JsonValue>,
}

impl LayerNodeProperties {
    pub fn new(id: u32, name: impl Into<String>, size: u32) -> Result<Self> {
        let name = name.into();
        validate_name(&name)?;
        let mask = blank_mask(size)?;
        Ok(Self {
            id,
            name,
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            disabled_channels: HashSet::new(),
            mask,
            extensions: JsonMap::new(),
        })
    }
}

/// A constant per-channel source. Empty values are invalid because they
/// produce a node with no editable output.
#[derive(Clone)]
pub struct FillLayer {
    pub properties: LayerNodeProperties,
    pub values: HashMap<Channel, [u8; 4]>,
}

/// A compositing container. Its children are ordered bottom-to-top.
#[derive(Clone)]
pub struct LayerGroup {
    pub properties: LayerNodeProperties,
    pub children: Vec<LayerNode>,
}

/// One paint, fill, or group node in a texture set's ordered layer tree.
#[derive(Clone)]
pub enum LayerNode {
    Paint(Layer),
    Fill(FillLayer),
    Group(LayerGroup),
}

impl LayerNode {
    pub fn mask(&self) -> &image::GrayImage {
        match self {
            Self::Paint(layer) => &layer.mask,
            Self::Fill(fill) => &fill.properties.mask,
            Self::Group(group) => &group.properties.mask,
        }
    }

    pub fn mask_mut(&mut self) -> &mut image::GrayImage {
        match self {
            Self::Paint(layer) => &mut layer.mask,
            Self::Fill(fill) => &mut fill.properties.mask,
            Self::Group(group) => &mut group.properties.mask,
        }
    }

    pub fn id(&self) -> u32 {
        match self {
            Self::Paint(layer) => layer.id,
            Self::Fill(fill) => fill.properties.id,
            Self::Group(group) => group.properties.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Paint(layer) => &layer.name,
            Self::Fill(fill) => &fill.properties.name,
            Self::Group(group) => &group.properties.name,
        }
    }

    pub fn is_paint_target(&self) -> bool {
        matches!(self, Self::Paint(_))
    }
}

/// One texture set's layer hierarchy. Root and child vectors are bottom-to-top.
#[derive(Clone)]
pub struct LayerTree {
    pub roots: Vec<LayerNode>,
    pub selected_node_id: u32,
    pub next_node_id: u32,
    /// Unknown fields on the recursive tree object retained by v9 round trips.
    pub extensions: JsonMap<String, JsonValue>,
}

impl LayerTree {
    /// Convert a legacy flat layer stack to the equivalent tree, preserving
    /// layer IDs, stack order, selection, and the next-ID counter.
    pub fn from_flat(layers: &[Layer], selected_index: usize, next_id: u32) -> Result<Self> {
        let decoded = layers.iter().try_fold(0u64, |sum, layer| {
            let bytes = layer
                .pixels
                .values()
                .try_fold(layer.mask.len() as u64, |sum, image| {
                    sum.checked_add(image.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))
                })?;
            sum.checked_add(bytes)
                .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))
        })?;
        if decoded
            .checked_mul(2)
            .is_none_or(|peak| peak > MAX_DECODED_BYTES)
        {
            bail!("Borrowed flat migration clone would exceed the 512-MiB allocation budget");
        }
        Self::from_flat_owned(layers.to_vec(), selected_index, next_id)
    }

    /// Consume a legacy flat stack without cloning any channel or mask buffers.
    pub fn from_flat_owned(
        layers: Vec<Layer>,
        selected_index: usize,
        next_id: u32,
    ) -> Result<Self> {
        let Some(first) = layers.first() else {
            bail!("Layer tree must contain at least one node");
        };
        if selected_index >= layers.len() {
            bail!("Selected layer index is out of range");
        }
        let size = first.mask.width();
        let selected_node_id = layers[selected_index].id;
        let tree = Self {
            roots: layers.into_iter().map(LayerNode::Paint).collect(),
            selected_node_id,
            next_node_id: next_id,
            extensions: JsonMap::new(),
        };
        tree.validate(size)?;
        if tree.checked_decoded_bytes()? > MAX_DECODED_BYTES {
            bail!("Flat layer migration exceeds the 512-MiB decoded texture budget");
        }
        Ok(tree)
    }

    /// Consume a Paint-only tree into the legacy flat adapter representation.
    pub fn into_flat_owned(self) -> Result<(Vec<Layer>, usize, u32)> {
        let selected_index = self
            .roots
            .iter()
            .position(|node| node.id() == self.selected_node_id)
            .ok_or_else(|| anyhow::anyhow!("Legacy flat conversion requires root selection"))?;
        let mut layers = Vec::new();
        layers.try_reserve_exact(self.roots.len())?;
        for node in self.roots {
            match node {
                LayerNode::Paint(layer) => layers.push(layer),
                _ => bail!("Legacy flat conversion cannot represent Fill or Group nodes"),
            }
        }
        Ok((layers, selected_index, self.next_node_id))
    }

    /// Validate IDs, structure, common metadata, fill values, and image sizes.
    pub fn validate(&self, size: u32) -> Result<()> {
        if !(64..=MAX_SIZE).contains(&size) {
            bail!("Invalid layer tree texture dimension");
        }
        if self.roots.is_empty() {
            bail!("Layer tree must contain at least one root node");
        }

        if self.roots.len() > MAX_LAYER_TREE_NODES {
            bail!("Layer tree exceeds the {MAX_LAYER_TREE_NODES}-node limit");
        }
        let mut stack = Vec::new();
        stack.try_reserve(self.roots.len())?;
        for node in self.roots.iter().rev() {
            stack.push((node, 1usize));
        }
        let mut ids = HashSet::new();
        ids.try_reserve(MAX_LAYER_TREE_NODES)?;
        let mut node_count = 0usize;
        let mut max_id = 0u32;

        while let Some((node, depth)) = stack.pop() {
            if depth > MAX_LAYER_TREE_DEPTH {
                bail!("Layer tree exceeds the {MAX_LAYER_TREE_DEPTH}-level depth limit");
            }
            node_count = node_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Layer tree node count overflow"))?;
            if node_count > MAX_LAYER_TREE_NODES {
                bail!("Layer tree exceeds the {MAX_LAYER_TREE_NODES}-node limit");
            }
            let id = node.id();
            if id == 0 || !ids.insert(id) {
                bail!("Layer tree contains a zero or duplicate node ID");
            }
            max_id = max_id.max(id);

            match node {
                LayerNode::Paint(layer) => {
                    validate_metadata(layer.id, &layer.name, layer.opacity, &layer.mask, size)?;
                    if layer
                        .pixels
                        .values()
                        .any(|image| image.dimensions() != (size, size))
                    {
                        bail!("Paint layer channel dimensions do not match its Texture Set");
                    }
                    if let Some(graph) = &layer.procedural_graph {
                        graph.validate()?;
                    }
                }
                LayerNode::Fill(fill) => {
                    validate_properties(&fill.properties, size)?;
                    if fill.values.is_empty() {
                        bail!("Fill layer must define at least one channel value");
                    }
                }
                LayerNode::Group(group) => {
                    validate_properties(&group.properties, size)?;
                    let pending = node_count
                        .checked_add(stack.len())
                        .and_then(|count| count.checked_add(group.children.len()))
                        .ok_or_else(|| anyhow::anyhow!("Layer tree node count overflow"))?;
                    if pending > MAX_LAYER_TREE_NODES {
                        bail!("Layer tree exceeds the {MAX_LAYER_TREE_NODES}-node limit");
                    }
                    stack.try_reserve(group.children.len())?;
                    for child in group.children.iter().rev() {
                        stack.push((child, depth + 1));
                    }
                }
            }
        }

        if self.next_node_id == 0 || self.next_node_id <= max_id {
            bail!("Next layer tree node ID must be greater than every node ID");
        }
        if self.find_node(self.selected_node_id).is_none() {
            bail!("Selected layer tree node ID does not exist");
        }
        if self.checked_decoded_bytes()? > MAX_DECODED_BYTES {
            bail!("Layer tree exceeds the 512-MiB decoded texture budget");
        }
        if tree_extension_storage_bytes(self) > MAX_LAYER_TREE_EXTENSION_BYTES {
            bail!("Layer tree extension metadata exceeds the 48-MiB limit");
        }
        Ok(())
    }

    /// Find a node by stable ID using bounded depth-first traversal.
    pub fn find_node(&self, id: u32) -> Option<&LayerNode> {
        find_node_in(&self.roots, id, 1)
    }

    pub fn find_node_mut(&mut self, id: u32) -> Option<&mut LayerNode> {
        find_node_in_mut(&mut self.roots, id, 1)
    }

    /// Return `Some(None)` for a root node, `Some(Some(parent_id))` for a
    /// nested node, and `None` when the ID is absent.
    pub fn parent_id(&self, id: u32) -> Option<Option<u32>> {
        parent_id_in(&self.roots, id, None, 1)
    }

    /// Return the sequence of child indices from `roots` to the requested node.
    pub fn path_to(&self, id: u32) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        path_to_in(&self.roots, id, 1, &mut path).then_some(path)
    }

    /// Return all node IDs in bottom-to-top depth-first order.
    pub fn all_node_ids(&self) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack: Vec<(&LayerNode, usize)> =
            self.roots.iter().rev().map(|node| (node, 1)).collect();
        while let Some((node, depth)) = stack.pop() {
            if depth > MAX_LAYER_TREE_DEPTH {
                continue;
            }
            result.push(node.id());
            if let LayerNode::Group(group) = node {
                stack.extend(group.children.iter().rev().map(|child| (child, depth + 1)));
            }
        }
        result
    }

    /// Return Paint leaf IDs in bottom-to-top depth-first compositing order.
    pub fn ordered_paint_leaf_ids(&self) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack: Vec<(&LayerNode, usize)> =
            self.roots.iter().rev().map(|node| (node, 1)).collect();
        while let Some((node, depth)) = stack.pop() {
            if depth > MAX_LAYER_TREE_DEPTH {
                continue;
            }
            match node {
                LayerNode::Paint(layer) => result.push(layer.id),
                LayerNode::Fill(_) => {}
                LayerNode::Group(group) => {
                    stack.extend(group.children.iter().rev().map(|child| (child, depth + 1)));
                }
            }
        }
        result
    }

    pub fn paint_leaf_ids(&self) -> Vec<u32> {
        self.ordered_paint_leaf_ids()
    }

    pub fn checked_decoded_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        let mut stack: Vec<&LayerNode> = self.roots.iter().collect();
        while let Some(node) = stack.pop() {
            let mask_len = match node {
                LayerNode::Paint(layer) => layer.mask.len(),
                LayerNode::Fill(fill) => fill.properties.mask.len(),
                LayerNode::Group(group) => group.properties.mask.len(),
            } as u64;
            total = total
                .checked_add(mask_len)
                .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
            if let LayerNode::Paint(layer) = node {
                for image in layer.pixels.values() {
                    total = total
                        .checked_add(image.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
                }
            }
            if let LayerNode::Group(group) = node {
                stack.try_reserve(group.children.len())?;
                stack.extend(group.children.iter());
            }
        }
        Ok(total)
    }

    /// Select any existing node; on failure the current selection is unchanged.
    pub fn select(&mut self, id: u32) -> Result<()> {
        if self.find_node(id).is_none() {
            bail!("Unknown layer tree node ID {id}");
        }
        self.selected_node_id = id;
        Ok(())
    }

    pub fn selected_paint_layer(&self) -> Option<&Layer> {
        match self.find_node(self.selected_node_id)? {
            LayerNode::Paint(layer) => Some(layer),
            LayerNode::Fill(_) | LayerNode::Group(_) => None,
        }
    }

    pub fn selected_paint_layer_mut(&mut self) -> Option<&mut Layer> {
        match self.find_node_mut(self.selected_node_id)? {
            LayerNode::Paint(layer) => Some(layer),
            LayerNode::Fill(_) | LayerNode::Group(_) => None,
        }
    }

    pub fn paint_layer(&self, id: u32) -> Option<&Layer> {
        match self.find_node(id)? {
            LayerNode::Paint(layer) => Some(layer),
            _ => None,
        }
    }

    pub fn paint_layer_mut(&mut self, id: u32) -> Option<&mut Layer> {
        match self.find_node_mut(id)? {
            LayerNode::Paint(layer) => Some(layer),
            _ => None,
        }
    }

    pub fn insert(&mut self, parent_id: Option<u32>, index: usize, node: LayerNode) -> Result<()> {
        let size = self.texture_size()?;
        self.validate(size)?;
        let sibling_count = self.siblings(parent_id)?.len();
        if index > sibling_count {
            bail!("Layer tree insertion index is out of range");
        }
        let subtree = summarize_subtree(&node, size)?;
        let decoded_bytes = self
            .checked_decoded_bytes()?
            .checked_add(subtree.decoded_bytes)
            .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
        if decoded_bytes > MAX_DECODED_BYTES {
            bail!("Inserted subtree exceeds the 512-MiB decoded texture budget");
        }
        if tree_extension_storage_bytes(self).saturating_add(subtree.extension_bytes)
            > MAX_LAYER_TREE_EXTENSION_BYTES
        {
            bail!("Inserted subtree exceeds the 48-MiB extension metadata limit");
        }
        if self
            .node_count()
            .checked_add(subtree.count)
            .is_none_or(|n| n > MAX_LAYER_TREE_NODES)
        {
            bail!("Layer tree exceeds the {MAX_LAYER_TREE_NODES}-node limit");
        }
        let start_depth = match parent_id {
            Some(parent) => self
                .path_to(parent)
                .map(|path| path.len() + 1)
                .ok_or_else(|| anyhow::anyhow!("Unknown layer tree parent ID {parent}"))?,
            None => 1,
        };
        if start_depth + subtree.height - 1 > MAX_LAYER_TREE_DEPTH {
            bail!("Layer tree exceeds the {MAX_LAYER_TREE_DEPTH}-level depth limit");
        }
        let existing_ids = self.all_node_ids();
        let mut seen = HashSet::new();
        seen.try_reserve(subtree.ids.len())?;
        for id in subtree.ids {
            if id == u32::MAX {
                bail!("Layer tree node ID counter exhausted");
            }
            if existing_ids.contains(&id) || !seen.insert(id) {
                bail!("Layer tree contains a zero or duplicate node ID");
            }
        }
        let next_id = self.next_node_id.max(
            seen.iter()
                .copied()
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Layer tree node ID counter exhausted"))?,
        );
        let siblings = self.siblings_mut(parent_id)?;
        siblings.try_reserve(1)?;
        siblings.insert(index, node);
        self.next_node_id = next_id;
        Ok(())
    }

    pub fn remove(&mut self, id: u32, selected_after_id: u32) -> Result<LayerNode> {
        self.validate(self.texture_size()?)?;
        let path = self
            .path_to(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown layer tree node ID {id}"))?;
        let parent = parent_id_in(&self.roots, id, None, 1)
            .ok_or_else(|| anyhow::anyhow!("Unknown layer tree node ID {id}"))?;
        let removed_ref = self.find_node(id).expect("path lookup found the node");
        if node_contains_id(removed_ref, selected_after_id)
            || self.find_node(selected_after_id).is_none()
        {
            bail!("Selected node after removal must exist outside the removed subtree");
        }
        let siblings = self.siblings_mut(parent)?;
        if siblings.len() == 1 && parent.is_none() {
            bail!("A layer tree must keep at least one root node");
        }
        let removed = siblings.remove(*path.last().expect("node path is nonempty"));
        if node_contains_id(&removed, self.selected_node_id) {
            self.selected_node_id = selected_after_id;
        }
        Ok(removed)
    }

    pub fn move_node(&mut self, id: u32, parent_id: Option<u32>, index: usize) -> Result<()> {
        self.validate(self.texture_size()?)?;
        let source_path = self
            .path_to(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown layer tree node ID {id}"))?;
        let source_parent = parent_id_in(&self.roots, id, None, 1)
            .ok_or_else(|| anyhow::anyhow!("Unknown layer tree node ID {id}"))?;
        if let Some(parent) = parent_id {
            if parent == id
                || self
                    .path_to(parent)
                    .is_some_and(|path| path.starts_with(&source_path))
            {
                bail!("Cannot move a layer node into itself or its descendant");
            }
            if !matches!(self.find_node(parent), Some(LayerNode::Group(_))) {
                bail!("Layer tree parent must be a Group node");
            }
        }
        let source_node = self.find_node(id).expect("path lookup found the node");
        let subtree = summarize_subtree(source_node, self.texture_size()?)?;
        let destination_len = self.siblings(parent_id)?.len();
        let same_siblings = source_parent == parent_id;
        // `index` is applied after detaching the source. For a move across
        // parents, the destination is unchanged before insertion, so its
        // valid range ends at its current length. Within one sibling list,
        // detaching first makes the valid end one slot smaller.
        let max_index = destination_len.saturating_sub(usize::from(same_siblings));
        if index > max_index {
            bail!("Layer tree insertion index is out of range");
        }
        if source_parent.is_none() && self.roots.len() == 1 {
            bail!("A layer tree must keep at least one root node");
        }
        let destination_depth = match parent_id {
            Some(parent) => self
                .path_to(parent)
                .map(|path| path.len() + 1)
                .ok_or_else(|| anyhow::anyhow!("Unknown layer tree parent ID {parent}"))?,
            None => 1,
        };
        if destination_depth + subtree.height - 1 > MAX_LAYER_TREE_DEPTH {
            bail!("Layer tree exceeds the {MAX_LAYER_TREE_DEPTH}-level depth limit");
        }

        let next_id = self.next_node_id.max(
            subtree
                .ids
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Layer tree node ID counter exhausted"))?,
        );

        // Reserve the destination before detaching the source. This keeps the
        // tree structurally unchanged if the allocation fails.
        if !same_siblings {
            self.siblings_mut(parent_id)?.try_reserve(1)?;
        }

        let node = take_path(&mut self.roots, &source_path)?;
        let siblings = self.siblings_mut(parent_id)?;
        siblings.insert(index, node);
        self.next_node_id = next_id;
        Ok(())
    }

    pub fn composite(&self, size: u32, channel: Channel) -> RgbaImage {
        self.composite_region(size, channel, 0, 0, size, size)
    }

    /// Composite only the selected node while preserving every ancestor Group's
    /// authored properties. Soloing a Group includes its complete subtree.
    pub fn composite_isolated(
        &self,
        node_id: u32,
        size: u32,
        channel: Channel,
    ) -> Option<RgbaImage> {
        self.composite_isolated_region(node_id, size, channel, 0, 0, size, size)
    }

    /// Region variant of [`LayerTree::composite_isolated`].
    #[allow(clippy::too_many_arguments)] // Mirrors the existing region API and adds a stable node ID.
    pub fn composite_isolated_region(
        &self,
        node_id: u32,
        size: u32,
        channel: Channel,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Option<RgbaImage> {
        let output_bytes = u64::from(width)
            .checked_mul(u64::from(height))?
            .checked_mul(4)?;
        if !(64..=MAX_SIZE).contains(&size)
            || width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|right| right > size)
            || y.checked_add(height).is_none_or(|bottom| bottom > size)
            || output_bytes > MAX_DECODED_BYTES
            || self.validate(size).is_err()
            || self
                .checked_decoded_bytes()
                .map_or(true, |bytes| bytes > MAX_DECODED_BYTES)
        {
            return None;
        }
        let indices = self.path_to(node_id)?;
        let path_len = indices.len();
        let mut nodes = &self.roots;
        let mut ids = Vec::new();
        ids.try_reserve_exact(path_len).ok()?;
        for index in indices {
            let node = nodes.get(index)?;
            ids.push(node.id());
            nodes = match node {
                LayerNode::Group(group) => &group.children,
                _ if ids.len() == path_len => break,
                _ => return None,
            };
        }
        Some(composite_roots(
            &self.roots,
            size,
            channel,
            x,
            y,
            width,
            height,
            Some(&ids),
        ))
    }

    pub fn composite_region(
        &self,
        size: u32,
        channel: Channel,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> RgbaImage {
        if !(64..=MAX_SIZE).contains(&size)
            || width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|right| right > size)
            || y.checked_add(height).is_none_or(|bottom| bottom > size)
        {
            return RgbaImage::new(0, 0);
        }
        if self.validate(size).is_err()
            || self
                .checked_decoded_bytes()
                .map_or(true, |bytes| bytes > MAX_DECODED_BYTES)
        {
            return RgbaImage::new(0, 0);
        }
        composite_roots(&self.roots, size, channel, x, y, width, height, None)
    }

    fn texture_size(&self) -> Result<u32> {
        let root = self
            .roots
            .first()
            .ok_or_else(|| anyhow::anyhow!("Layer tree has no roots"))?;
        Ok(match root {
            LayerNode::Paint(layer) => layer.mask.width(),
            LayerNode::Fill(fill) => fill.properties.mask.width(),
            LayerNode::Group(group) => group.properties.mask.width(),
        })
    }

    fn siblings_mut(&mut self, parent_id: Option<u32>) -> Result<&mut Vec<LayerNode>> {
        match parent_id {
            None => Ok(&mut self.roots),
            Some(id) => match self.find_node_mut(id) {
                Some(LayerNode::Group(group)) => Ok(&mut group.children),
                Some(_) => bail!("Layer tree parent must be a Group node"),
                None => bail!("Unknown layer tree parent ID {id}"),
            },
        }
    }

    pub(crate) fn siblings(&self, parent_id: Option<u32>) -> Result<&Vec<LayerNode>> {
        match parent_id {
            None => Ok(&self.roots),
            Some(id) => match self.find_node(id) {
                Some(LayerNode::Group(group)) => Ok(&group.children),
                Some(_) => bail!("Layer tree parent must be a Group node"),
                None => bail!("Unknown layer tree parent ID {id}"),
            },
        }
    }

    fn node_count(&self) -> usize {
        self.all_node_ids().len()
    }

    /// Allocate a new fill node with a fresh stable ID.
    pub fn allocate_fill(
        &mut self,
        name: impl Into<String>,
        size: u32,
        values: HashMap<Channel, [u8; 4]>,
    ) -> Result<FillLayer> {
        if values.is_empty() {
            bail!("Fill layer must define at least one channel value");
        }
        let name = name.into();
        let (id, next_id) = self.peek_node_id()?;
        let properties = LayerNodeProperties::new(id, name, size)?;
        self.next_node_id = next_id;
        Ok(FillLayer { properties, values })
    }

    /// Allocate a new empty group with a fresh stable ID.
    pub fn allocate_group(&mut self, name: impl Into<String>, size: u32) -> Result<LayerGroup> {
        let (id, next_id) = self.peek_node_id()?;
        let properties = LayerNodeProperties::new(id, name, size)?;
        self.next_node_id = next_id;
        Ok(LayerGroup {
            properties,
            children: Vec::new(),
        })
    }

    fn peek_node_id(&self) -> Result<(u32, u32)> {
        let id = self.next_node_id;
        if id == 0 || self.find_node(id).is_some() {
            bail!("Layer tree next node ID is invalid or already in use");
        }
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Layer tree node ID counter exhausted"))?;
        Ok((id, next_id))
    }
}

#[derive(Clone, Copy)]
struct CompositePixel {
    value: [u8; 4],
    coverage: f32,
}

#[allow(clippy::too_many_arguments)] // Explicit render bounds and optional isolation path stay allocation-free.
fn composite_roots(
    roots: &[LayerNode],
    size: u32,
    channel: Channel,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    solo_path: Option<&[u32]>,
) -> RgbaImage {
    let mut out = RgbaImage::new(width, height);
    let mut generated = HashMap::<u32, (u32, Vec<u8>)>::new();
    for local_y in 0..height {
        generated.clear();
        if channel == Channel::Roughness {
            let mut stack: Vec<&LayerNode> = roots.iter().collect();
            while let Some(node) = stack.pop() {
                match node {
                    LayerNode::Paint(layer) => {
                        if let Some(graph) = layer.procedural_graph.as_ref().filter(|graph| {
                            graph.validate().is_ok()
                                && layer.visible
                                && layer.opacity > 0.0
                                && !layer.disabled_channels.contains(&channel)
                        }) {
                            let occlusion = graph
                                .occlusion_source_layer_id()
                                .and_then(|id| find_paint_layer(roots, id))
                                .and_then(|layer| layer.pixels.get(&Channel::Occlusion))
                                .map(|image| image.as_raw().as_slice());
                            if let Ok(row) =
                                graph.render_roughness_span(size, y + local_y, x, width, occlusion)
                            {
                                generated.insert(layer.id, (x, row));
                            }
                        }
                    }
                    LayerNode::Group(group) => stack.extend(group.children.iter()),
                    LayerNode::Fill(_) => {}
                }
            }
        }
        for local_x in 0..width {
            let px = x + local_x;
            let py = y + local_y;
            let pixel =
                composite_siblings_pixel(roots, channel, px, py, &generated, true, 1, solo_path);
            out.put_pixel(local_x, local_y, Rgba(pixel.value));
        }
    }
    out
}

#[allow(clippy::too_many_arguments)] // Pixel traversal carries coverage, depth, root and isolation context.
fn composite_siblings_pixel(
    nodes: &[LayerNode],
    channel: Channel,
    x: u32,
    y: u32,
    generated: &HashMap<u32, (u32, Vec<u8>)>,
    root: bool,
    depth: usize,
    solo_path: Option<&[u32]>,
) -> CompositePixel {
    let mut destination = CompositePixel {
        value: channel.background(),
        coverage: if root { 1.0 } else { 0.0 },
    };
    if depth > MAX_LAYER_TREE_DEPTH {
        return destination;
    }
    for node in nodes {
        if solo_path.is_some_and(|path| path.first().copied() != Some(node.id())) {
            continue;
        }
        match node {
            LayerNode::Paint(layer) => {
                if !layer.visible
                    || layer.opacity <= 0.0
                    || layer.disabled_channels.contains(&channel)
                {
                    continue;
                }
                let mask = layer.mask.get_pixel(x, y).0[0] as f32 / 255.0;
                let image_source = layer
                    .pixels
                    .get(&channel)
                    .map(|image| image.get_pixel(x, y).0);
                let source = if channel == Channel::Roughness {
                    generated_roughness_pixel(layer, image_source, x, generated)
                } else {
                    image_source
                };
                if let Some(source) = source {
                    let coverage = (source[3] as f32 / 255.0) * layer.opacity * mask;
                    blend_composite_pixel(&mut destination, source, coverage, channel, layer.blend);
                }
            }
            LayerNode::Fill(fill) => {
                let properties = &fill.properties;
                if !properties.visible
                    || properties.opacity <= 0.0
                    || properties.disabled_channels.contains(&channel)
                {
                    continue;
                }
                if let Some(source) = fill.values.get(&channel).copied() {
                    let mask = properties.mask.get_pixel(x, y).0[0] as f32 / 255.0;
                    let coverage = source[3] as f32 / 255.0 * properties.opacity * mask;
                    blend_composite_pixel(
                        &mut destination,
                        source,
                        coverage,
                        channel,
                        properties.blend,
                    );
                }
            }
            LayerNode::Group(group) => {
                let properties = &group.properties;
                if !properties.visible
                    || properties.opacity <= 0.0
                    || properties.disabled_channels.contains(&channel)
                {
                    continue;
                }
                let source = composite_siblings_pixel(
                    &group.children,
                    channel,
                    x,
                    y,
                    generated,
                    false,
                    depth + 1,
                    solo_path.and_then(|path| (path.len() > 1).then_some(&path[1..])),
                );
                let mask = properties.mask.get_pixel(x, y).0[0] as f32 / 255.0;
                blend_composite_pixel(
                    &mut destination,
                    source.value,
                    source.coverage * properties.opacity * mask,
                    channel,
                    properties.blend,
                );
            }
        }
    }
    destination.value[3] = (destination.coverage * 255.0).round().clamp(0.0, 255.0) as u8;
    destination
}

fn generated_roughness_pixel(
    layer: &Layer,
    authored: Option<[u8; 4]>,
    x: u32,
    generated: &HashMap<u32, (u32, Vec<u8>)>,
) -> Option<[u8; 4]> {
    let Some((origin, row)) = generated.get(&layer.id) else {
        return authored;
    };
    let index = usize::try_from(x.checked_sub(*origin)?)
        .ok()?
        .checked_mul(4)?;
    let generated_value = *row.get(index)?;
    let value = if let Some(authored) = authored {
        let alpha = authored[3] as f32 / 255.0;
        (generated_value as f32 + (authored[0] as f32 - generated_value as f32) * alpha)
            .round()
            .clamp(0.0, 255.0) as u8
    } else {
        generated_value
    };
    Some([value, value, value, 255])
}

fn blend_composite_pixel(
    destination: &mut CompositePixel,
    source: [u8; 4],
    source_coverage: f32,
    channel: Channel,
    mode: BlendMode,
) {
    let source_coverage = if source_coverage.is_finite() {
        source_coverage.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if source_coverage <= 0.0 {
        return;
    }
    let dst_coverage = destination.coverage.clamp(0.0, 1.0);
    let out_coverage = source_coverage + dst_coverage * (1.0 - source_coverage);
    if out_coverage <= 1.0e-8 {
        return;
    }
    match channel {
        Channel::BaseColor | Channel::Emissive => {
            let src = std::array::from_fn(|c| srgb_to_linear(source[c] as f32 / 255.0));
            let dst = std::array::from_fn(|c| srgb_to_linear(destination.value[c] as f32 / 255.0));
            let blended = crate::blend::blend_rgb(mode, dst, src);
            for c in 0..3 {
                let (src, dst, blended) = (src[c], dst[c], blended[c]);
                if dst_coverage >= 1.0 {
                    let linear = dst + (blended - dst) * source_coverage;
                    destination.value[c] =
                        (linear_to_srgb(linear) * 255.0).round().clamp(0.0, 255.0) as u8;
                    continue;
                }
                let numerator = source_coverage * (1.0 - dst_coverage) * src
                    + source_coverage * dst_coverage * blended
                    + (1.0 - source_coverage) * dst_coverage * dst;
                destination.value[c] = (linear_to_srgb(numerator / out_coverage) * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
        Channel::Normal => {
            let dst = std::array::from_fn(|c| destination.value[c] as f32 / 255.0);
            let src = std::array::from_fn(|c| source[c] as f32 / 255.0);
            let normal = crate::blend::blend_normal(mode, dst, src, source_coverage, dst_coverage);
            for (component, normal_component) in destination.value[..3].iter_mut().zip(normal) {
                *component = ((normal_component * 0.5 + 0.5) * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
        _ => {
            for (c, source_component) in source[..3].iter().enumerate() {
                let src = *source_component as f32 / 255.0;
                let dst = destination.value[c] as f32 / 255.0;
                let blended = crate::blend::blend_rgb(mode, [dst; 3], [src; 3])[0];
                if dst_coverage >= 1.0 {
                    destination.value[c] = ((dst + (blended - dst) * source_coverage) * 255.0)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    continue;
                }
                let numerator = source_coverage * (1.0 - dst_coverage) * src
                    + source_coverage * dst_coverage * blended
                    + (1.0 - source_coverage) * dst_coverage * dst;
                destination.value[c] = ((numerator / out_coverage) * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
    destination.coverage = out_coverage;
}

fn validate_properties(properties: &LayerNodeProperties, size: u32) -> Result<()> {
    validate_metadata(
        properties.id,
        &properties.name,
        properties.opacity,
        &properties.mask,
        size,
    )
}

fn validate_metadata(id: u32, name: &str, opacity: f32, mask: &GrayImage, size: u32) -> Result<()> {
    if id == 0 {
        bail!("Layer tree node ID must be nonzero");
    }
    validate_name(name)?;
    if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
        bail!("Layer tree node opacity must be finite and within 0..=1");
    }
    if mask.dimensions() != (size, size) {
        bail!("Layer tree node mask dimensions do not match its Texture Set");
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 1024 {
        bail!("Layer tree node name must contain 1–1024 bytes");
    }
    Ok(())
}

struct SubtreeSummary {
    count: usize,
    height: usize,
    ids: Vec<u32>,
    decoded_bytes: u64,
    extension_bytes: usize,
}

fn summarize_subtree(root: &LayerNode, size: u32) -> Result<SubtreeSummary> {
    let mut stack = vec![(root, 1usize)];
    let mut ids = Vec::new();
    ids.try_reserve(1)?;
    let mut seen = HashSet::new();
    seen.try_reserve(1)?;
    let mut height = 0usize;
    let mut decoded_bytes = 0u64;
    let mut extension_bytes = 0usize;
    let extension_cap = MAX_LAYER_TREE_EXTENSION_BYTES.saturating_add(1);
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_LAYER_TREE_DEPTH {
            bail!("Layer tree exceeds the {MAX_LAYER_TREE_DEPTH}-level depth limit");
        }
        if ids.len() >= MAX_LAYER_TREE_NODES {
            bail!("Layer tree exceeds the {MAX_LAYER_TREE_NODES}-node limit");
        }
        if node.id() == 0 || !seen.insert(node.id()) {
            bail!("Layer tree contains a zero or duplicate node ID");
        }
        ids.try_reserve(1)?;
        ids.push(node.id());
        height = height.max(depth);
        match node {
            LayerNode::Paint(layer) => {
                validate_metadata(layer.id, &layer.name, layer.opacity, &layer.mask, size)?;
                if layer
                    .pixels
                    .values()
                    .any(|image| image.dimensions() != (size, size))
                {
                    bail!("Paint layer channel dimensions do not match its Texture Set");
                }
                if let Some(graph) = &layer.procedural_graph {
                    graph.validate()?;
                }
                decoded_bytes = decoded_bytes
                    .checked_add(layer.mask.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
                for image in layer.pixels.values() {
                    decoded_bytes = decoded_bytes
                        .checked_add(image.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
                }
                extension_bytes = extension_bytes
                    .saturating_add(json_map_storage_bytes(&layer.extensions, extension_cap))
                    .min(extension_cap);
            }
            LayerNode::Fill(fill) => {
                validate_properties(&fill.properties, size)?;
                if fill.values.is_empty() {
                    bail!("Fill layer must define at least one channel value");
                }
                decoded_bytes = decoded_bytes
                    .checked_add(fill.properties.mask.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
                extension_bytes = extension_bytes
                    .saturating_add(json_map_storage_bytes(
                        &fill.properties.extensions,
                        extension_cap,
                    ))
                    .min(extension_cap);
            }
            LayerNode::Group(group) => {
                validate_properties(&group.properties, size)?;
                stack.try_reserve(group.children.len())?;
                for child in group.children.iter().rev() {
                    stack.push((child, depth + 1));
                }
                decoded_bytes = decoded_bytes
                    .checked_add(group.properties.mask.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Texture byte count overflow"))?;
                extension_bytes = extension_bytes
                    .saturating_add(json_map_storage_bytes(
                        &group.properties.extensions,
                        extension_cap,
                    ))
                    .min(extension_cap);
            }
        }
    }
    Ok(SubtreeSummary {
        count: ids.len(),
        height,
        ids,
        decoded_bytes,
        extension_bytes,
    })
}

fn node_contains_id(node: &LayerNode, id: u32) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if node.id() == id {
            return true;
        }
        if let LayerNode::Group(group) = node {
            stack.extend(group.children.iter());
        }
    }
    false
}

fn tree_extension_storage_bytes(tree: &LayerTree) -> usize {
    let cap = MAX_LAYER_TREE_EXTENSION_BYTES.saturating_add(1);
    let mut total = json_map_storage_bytes(&tree.extensions, cap);
    let mut stack: Vec<&LayerNode> = tree.roots.iter().collect();
    while let Some(node) = stack.pop() {
        let extensions = match node {
            LayerNode::Paint(layer) => &layer.extensions,
            LayerNode::Fill(fill) => &fill.properties.extensions,
            LayerNode::Group(group) => {
                stack.extend(group.children.iter());
                &group.properties.extensions
            }
        };
        total = total.saturating_add(json_map_storage_bytes(extensions, cap));
        if total >= cap {
            return cap;
        }
    }
    total
}

fn json_map_storage_bytes(map: &JsonMap<String, JsonValue>, cap: usize) -> usize {
    map.iter().fold(0usize, |total, (key, value)| {
        total
            .saturating_add(key.len())
            .saturating_add(json_value_storage_bytes(value, cap))
            .min(cap)
    })
}

fn json_value_storage_bytes(value: &JsonValue, cap: usize) -> usize {
    match value {
        JsonValue::Null => 0,
        JsonValue::Bool(_) | JsonValue::Number(_) => 8,
        JsonValue::String(value) => value.len(),
        JsonValue::Array(values) => values.iter().fold(0usize, |total, value| {
            total
                .saturating_add(json_value_storage_bytes(value, cap))
                .min(cap)
        }),
        JsonValue::Object(values) => json_map_storage_bytes(values, cap),
    }
}

fn find_paint_layer(nodes: &[LayerNode], id: u32) -> Option<&Layer> {
    let mut stack: Vec<&LayerNode> = nodes.iter().rev().collect();
    while let Some(node) = stack.pop() {
        match node {
            LayerNode::Paint(layer) if layer.id == id => return Some(layer),
            LayerNode::Group(group) => stack.extend(group.children.iter().rev()),
            _ => {}
        }
    }
    None
}

fn take_node(tree: &mut LayerTree, id: u32) -> Result<LayerNode> {
    let path = tree
        .path_to(id)
        .ok_or_else(|| anyhow::anyhow!("Unknown layer tree node ID {id}"))?;
    take_path(&mut tree.roots, &path)
}

fn take_path(nodes: &mut Vec<LayerNode>, path: &[usize]) -> Result<LayerNode> {
    let Some((&index, rest)) = path.split_first() else {
        bail!("Layer tree node path is empty");
    };
    if index >= nodes.len() {
        bail!("Layer tree node path is out of range");
    }
    if rest.is_empty() {
        return Ok(nodes.remove(index));
    }
    match &mut nodes[index] {
        LayerNode::Group(group) => take_path(&mut group.children, rest),
        _ => bail!("Layer tree path traverses a non-Group node"),
    }
}

fn blank_mask(size: u32) -> Result<GrayImage> {
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid layer tree texture dimension");
    }
    let byte_len = (size as usize)
        .checked_mul(size as usize)
        .ok_or_else(|| anyhow::anyhow!("Layer tree mask dimensions overflow"))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(byte_len)?;
    bytes.resize(byte_len, 255);
    ImageBuffer::from_raw(size, size, bytes)
        .ok_or_else(|| anyhow::anyhow!("Could not construct layer tree mask"))
}

fn find_node_in(nodes: &[LayerNode], id: u32, depth: usize) -> Option<&LayerNode> {
    if depth > MAX_LAYER_TREE_DEPTH {
        return None;
    }
    for node in nodes {
        if node.id() == id {
            return Some(node);
        }
        if let LayerNode::Group(group) = node {
            if let Some(found) = find_node_in(&group.children, id, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn find_node_in_mut(nodes: &mut [LayerNode], id: u32, depth: usize) -> Option<&mut LayerNode> {
    if depth > MAX_LAYER_TREE_DEPTH {
        return None;
    }
    for node in nodes {
        if node.id() == id {
            return Some(node);
        }
        if let LayerNode::Group(group) = node {
            if let Some(found) = find_node_in_mut(&mut group.children, id, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn parent_id_in(
    nodes: &[LayerNode],
    id: u32,
    parent: Option<u32>,
    depth: usize,
) -> Option<Option<u32>> {
    if depth > MAX_LAYER_TREE_DEPTH {
        return None;
    }
    for node in nodes {
        if node.id() == id {
            return Some(parent);
        }
        if let LayerNode::Group(group) = node {
            if let Some(found) =
                parent_id_in(&group.children, id, Some(group.properties.id), depth + 1)
            {
                return Some(found);
            }
        }
    }
    None
}

fn path_to_in(nodes: &[LayerNode], id: u32, depth: usize, path: &mut Vec<usize>) -> bool {
    if depth > MAX_LAYER_TREE_DEPTH {
        return false;
    }
    for (index, node) in nodes.iter().enumerate() {
        path.push(index);
        if node.id() == id {
            return true;
        }
        if let LayerNode::Group(group) = node {
            if path_to_in(&group.children, id, depth + 1, path) {
                return true;
            }
        }
        path.pop();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    fn flat_tree() -> LayerTree {
        let layers = vec![Layer::new(1, "Bottom", 64), Layer::new(2, "Top", 64)];
        LayerTree::from_flat(&layers, 1, 3).unwrap()
    }

    #[test]
    fn flat_migration_preserves_ids_order_selection_and_next_id() {
        let mut layers = vec![Layer::new(7, "Bottom", 64), Layer::new(11, "Top", 64)];
        layers[0].visible = false;
        layers[1].mask.put_pixel(2, 3, Luma([91]));
        let tree = LayerTree::from_flat(&layers, 1, 12).unwrap();
        assert_eq!(
            tree.roots.iter().map(LayerNode::id).collect::<Vec<_>>(),
            [7, 11]
        );
        assert_eq!(tree.selected_node_id, 11);
        assert_eq!(tree.next_node_id, 12);
        assert!(!matches!(&tree.roots[0], LayerNode::Paint(layer) if layer.visible));
        assert!(
            matches!(&tree.roots[1], LayerNode::Paint(layer) if layer.mask.get_pixel(2, 3).0 == [91])
        );
    }

    #[test]
    fn traversal_lookup_parent_path_and_paint_leaf_order_are_depth_first() {
        let mut tree = flat_tree();
        let group = tree.allocate_group("Group", 64).unwrap();
        let group_id = group.properties.id;
        tree.roots.insert(1, LayerNode::Group(group));
        let nested = Layer::new(4, "Nested", 64);
        if let LayerNode::Group(group) = &mut tree.roots[1] {
            group.children.push(LayerNode::Paint(nested));
        }
        tree.next_node_id = 5;

        assert_eq!(tree.find_node(4).map(LayerNode::name), Some("Nested"));
        assert_eq!(tree.parent_id(1), Some(None));
        assert_eq!(tree.parent_id(4), Some(Some(group_id)));
        assert_eq!(tree.path_to(4), Some(vec![1, 0]));
        assert_eq!(tree.ordered_paint_leaf_ids(), [1, 4, 2]);
        tree.validate(64).unwrap();
    }

    #[test]
    fn groups_cannot_be_selected_as_paint_targets() {
        let mut tree = flat_tree();
        let group = tree.allocate_group("Folder", 64).unwrap();
        let group_id = group.properties.id;
        tree.roots.push(LayerNode::Group(group));
        let selected_before = tree.selected_node_id;
        assert!(tree.select(group_id).is_ok());
        assert_eq!(tree.selected_node_id, group_id);
        assert!(tree.selected_paint_layer().is_none());
        assert!(tree.select(2).is_ok());
        assert_eq!(tree.selected_node_id, 2);
        assert!(tree.selected_paint_layer().is_some());
        assert_ne!(selected_before, group_id);
    }

    #[test]
    fn checked_insert_move_remove_and_nested_paint_lookup() {
        let mut tree = flat_tree();
        let group = tree.allocate_group("Folder", 64).unwrap();
        let group_id = group.properties.id;
        tree.insert(None, 1, LayerNode::Group(group)).unwrap();
        let nested = Layer::new(tree.next_node_id, "Nested", 64);
        let nested_id = nested.id;
        tree.insert(Some(group_id), 0, LayerNode::Paint(nested))
            .unwrap();
        assert!(tree.paint_layer(nested_id).is_some());
        assert_eq!(tree.parent_id(nested_id), Some(Some(group_id)));
        assert_eq!(tree.all_node_ids(), [1, group_id, nested_id, 2]);
        assert_eq!(tree.ordered_paint_leaf_ids(), [1, nested_id, 2]);

        let before = tree.all_node_ids();
        assert!(tree.move_node(group_id, Some(group_id), 0).is_err());
        assert_eq!(
            tree.all_node_ids(),
            before,
            "cycle rejection must not mutate"
        );
        assert!(tree.move_node(nested_id, Some(1), 0).is_err());
        assert_eq!(
            tree.all_node_ids(),
            before,
            "non-Group parent rejection must not mutate"
        );
        assert!(tree.move_node(nested_id, None, usize::MAX).is_err());
        assert_eq!(
            tree.all_node_ids(),
            before,
            "invalid destination index must not mutate"
        );

        tree.move_node(nested_id, None, 2).unwrap();
        assert_eq!(tree.parent_id(nested_id), Some(None));
        assert_eq!(tree.all_node_ids(), [1, group_id, nested_id, 2]);
        tree.select(group_id).unwrap();
        let before_remove = tree.all_node_ids();
        assert!(tree.remove(group_id, u32::MAX).is_err());
        assert_eq!(tree.all_node_ids(), [1, group_id, nested_id, 2]);
        assert_eq!(tree.all_node_ids(), before_remove);
        let removed_group = tree.remove(group_id, nested_id).unwrap();
        assert_eq!(removed_group.id(), group_id);
        assert_eq!(tree.selected_node_id, nested_id);
        let removed = tree.remove(2, 1).unwrap();
        assert_eq!(removed.id(), 2);
        assert_eq!(tree.all_node_ids(), [1, nested_id]);
        tree.validate(64).unwrap();
    }

    #[test]
    fn cross_parent_move_rejects_one_past_end_without_mutation() {
        let mut tree = flat_tree();
        let group = tree.allocate_group("Folder", 64).unwrap();
        let group_id = group.properties.id;
        tree.insert(None, 1, LayerNode::Group(group)).unwrap();
        let nested = Layer::new(tree.next_node_id, "Nested", 64);
        let nested_id = nested.id;
        tree.insert(Some(group_id), 0, LayerNode::Paint(nested))
            .unwrap();

        let snapshot = |tree: &LayerTree| {
            let locations = tree
                .all_node_ids()
                .into_iter()
                .map(|id| {
                    let parent = tree.parent_id(id).unwrap();
                    let index = tree
                        .siblings(parent)
                        .unwrap()
                        .iter()
                        .position(|node| node.id() == id)
                        .unwrap();
                    (id, parent, index)
                })
                .collect::<Vec<_>>();
            (locations, tree.selected_node_id, tree.next_node_id)
        };

        for (node_id, destination, index) in [
            (1, Some(group_id), 2), // Group has one child; len + 1 is invalid.
            (nested_id, None, 4),   // Root has three children; len + 1 is invalid.
            (1, Some(group_id), usize::MAX),
            (nested_id, None, usize::MAX),
        ] {
            let before = snapshot(&tree);
            assert!(tree.move_node(node_id, destination, index).is_err());
            assert_eq!(
                snapshot(&tree),
                before,
                "rejected cross-parent move must preserve the complete tree"
            );
            tree.validate(64).unwrap();
        }

        // The exact destination end remains valid in either direction.
        tree.move_node(1, Some(group_id), 1).unwrap();
        tree.move_node(1, None, tree.siblings(None).unwrap().len())
            .unwrap();
        tree.validate(64).unwrap();
    }

    #[test]
    fn insert_preflights_depth_count_and_duplicate_ids_without_mutation() {
        let mut tree = flat_tree();
        let before = tree.all_node_ids();
        let mut deep = LayerNode::Paint(Layer::new(10, "Deep", 64));
        for id in (11..=MAX_LAYER_TREE_DEPTH as u32 + 10).rev() {
            deep = LayerNode::Group(LayerGroup {
                properties: LayerNodeProperties::new(id, format!("G{id}"), 64).unwrap(),
                children: vec![deep],
            });
        }
        assert!(tree.insert(None, 0, deep).is_err());
        assert_eq!(tree.all_node_ids(), before);
        assert!(tree
            .insert(None, 0, LayerNode::Paint(Layer::new(1, "Duplicate", 64)))
            .is_err());
        assert_eq!(tree.all_node_ids(), before);

        let mut full = LayerTree {
            roots: (1..=MAX_LAYER_TREE_NODES as u32)
                .map(|id| LayerNode::Paint(Layer::new(id, "L", 64)))
                .collect(),
            selected_node_id: 1,
            next_node_id: MAX_LAYER_TREE_NODES as u32 + 1,
            extensions: JsonMap::new(),
        };
        let full_before = full.all_node_ids();
        assert!(full
            .insert(
                None,
                0,
                LayerNode::Paint(Layer::new(full.next_node_id, "Over", 64))
            )
            .is_err());
        assert_eq!(full.all_node_ids(), full_before);
    }

    #[test]
    fn insert_preflights_aggregate_extensions_without_mutation() {
        let mut tree = flat_tree();
        let before_ids = tree.all_node_ids();
        let before_selection = tree.selected_node_id;
        let before_next_id = tree.next_node_id;
        let mut paint = Layer::new(tree.next_node_id, "Future metadata", 64);
        paint.extensions.insert(
            "future".into(),
            JsonValue::String("x".repeat(MAX_LAYER_TREE_EXTENSION_BYTES + 1)),
        );

        assert!(tree
            .insert(None, tree.roots.len(), LayerNode::Paint(paint))
            .is_err());
        assert_eq!(tree.all_node_ids(), before_ids);
        assert_eq!(tree.selected_node_id, before_selection);
        assert_eq!(tree.next_node_id, before_next_id);
        tree.validate(64).unwrap();
    }

    #[test]
    fn moving_into_a_full_depth_group_rejects_without_mutation() {
        let mut deepest = LayerNode::Group(LayerGroup {
            properties: LayerNodeProperties::new(MAX_LAYER_TREE_DEPTH as u32, "G", 64).unwrap(),
            children: Vec::new(),
        });
        for id in (1..MAX_LAYER_TREE_DEPTH as u32).rev() {
            deepest = LayerNode::Group(LayerGroup {
                properties: LayerNodeProperties::new(id, format!("G{id}"), 64).unwrap(),
                children: vec![deepest],
            });
        }
        let deepest_group_id = MAX_LAYER_TREE_DEPTH as u32;
        let mut tree = LayerTree {
            roots: vec![deepest, LayerNode::Paint(Layer::new(33, "Root", 64))],
            selected_node_id: 33,
            next_node_id: 34,
            extensions: JsonMap::new(),
        };
        let before_ids = tree.all_node_ids();
        let before_selection = tree.selected_node_id;
        let before_next_id = tree.next_node_id;
        assert!(tree.move_node(33, Some(deepest_group_id), 0).is_err());
        assert_eq!(tree.all_node_ids(), before_ids);
        assert_eq!(tree.selected_node_id, before_selection);
        assert_eq!(tree.next_node_id, before_next_id);
        tree.validate(64).unwrap();
    }

    #[test]
    fn decoded_budget_counts_paint_channels_and_all_node_masks() {
        let mut tree = flat_tree();
        let group = tree.allocate_group("Folder", 64).unwrap();
        let group_id = group.properties.id;
        tree.insert(None, 1, LayerNode::Group(group)).unwrap();
        let mut fill_values = HashMap::new();
        fill_values.insert(Channel::BaseColor, [200, 100, 50, 255]);
        let fill = FillLayer {
            properties: LayerNodeProperties::new(tree.next_node_id, "Fill", 64).unwrap(),
            values: fill_values,
        };
        tree.insert(Some(group_id), 0, LayerNode::Fill(fill))
            .unwrap();
        tree.paint_layer_mut(1)
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64);
        assert_eq!(
            tree.checked_decoded_bytes().unwrap(),
            64 * 64 * (2 + 1 + 1 + 4)
        );
    }

    #[test]
    fn isolated_group_opacity_and_mask_apply_once_with_full_region_parity() {
        let mut tree = flat_tree();
        tree.roots.truncate(1);
        tree.selected_node_id = 1;
        let mut group = tree.allocate_group("Isolated", 64).unwrap();
        group.properties.opacity = 0.5;
        group.properties.mask.put_pixel(4, 7, Luma([128]));
        let mut paint = Layer::new(tree.next_node_id, "Half-alpha red", 64);
        paint
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 7, image::Rgba([255, 0, 0, 128]));
        let paint_id = paint.id;
        group.children.push(LayerNode::Paint(paint));
        tree.insert(None, 1, LayerNode::Group(group)).unwrap();
        tree.select(paint_id).unwrap();
        let full = tree.composite(64, Channel::BaseColor);
        let region = tree.composite_region(64, Channel::BaseColor, 3, 6, 4, 3);
        for local_y in 0..3 {
            for local_x in 0..4 {
                assert_eq!(
                    region.get_pixel(local_x, local_y),
                    full.get_pixel(local_x + 3, local_y + 6)
                );
            }
        }
        let alpha = (128.0 / 255.0) * 0.5 * (128.0 / 255.0);
        let background = [190_u8, 185, 202];
        let expected = [
            (linear_to_srgb(srgb_to_linear(background[0] as f32 / 255.0) * (1.0 - alpha) + alpha)
                * 255.0)
                .round() as u8,
            (linear_to_srgb(srgb_to_linear(background[1] as f32 / 255.0) * (1.0 - alpha)) * 255.0)
                .round() as u8,
            (linear_to_srgb(srgb_to_linear(background[2] as f32 / 255.0) * (1.0 - alpha)) * 255.0)
                .round() as u8,
        ];
        let actual = full.get_pixel(4, 7).0;
        assert_eq!(&actual[..3], &expected);
        assert_eq!(actual[3], 255);
    }

    #[test]
    fn nested_fill_groups_compose_masks_opacity_and_disabled_channels_once() {
        let mut tree = flat_tree();
        tree.roots.truncate(1);
        tree.selected_node_id = 1;
        let mut outer = tree.allocate_group("Outer", 64).unwrap();
        outer.properties.opacity = 0.5;
        outer.properties.mask.put_pixel(8, 9, Luma([128]));
        let outer_id = outer.properties.id;
        let mut inner = LayerGroup {
            properties: LayerNodeProperties::new(tree.next_node_id + 1, "Inner", 64).unwrap(),
            children: Vec::new(),
        };
        inner.properties.opacity = 0.5;
        inner.properties.mask.put_pixel(8, 9, Luma([128]));
        let mut values = HashMap::new();
        values.insert(Channel::BaseColor, [210, 100, 50, 255]);
        values.insert(Channel::Normal, [190, 130, 220, 255]);
        let fill = FillLayer {
            properties: LayerNodeProperties::new(tree.next_node_id + 2, "Color fill", 64).unwrap(),
            values,
        };
        inner.children.push(LayerNode::Fill(fill));
        outer.children.push(LayerNode::Group(inner));
        tree.insert(None, 1, LayerNode::Group(outer)).unwrap();

        let full = tree.composite(64, Channel::BaseColor);
        let region = tree.composite_region(64, Channel::BaseColor, 6, 7, 5, 4);
        for local_y in 0..4 {
            for local_x in 0..5 {
                assert_eq!(
                    region.get_pixel(local_x, local_y),
                    full.get_pixel(local_x + 6, local_y + 7)
                );
            }
        }
        let coverage = 0.5 * (128.0 / 255.0) * 0.5 * (128.0 / 255.0);
        let background = Channel::BaseColor.background();
        let source = [210_u8, 100, 50];
        let expected = std::array::from_fn::<_, 3, _>(|index| {
            let bg = srgb_to_linear(background[index] as f32 / 255.0);
            let src = srgb_to_linear(source[index] as f32 / 255.0);
            (linear_to_srgb(bg * (1.0 - coverage) + src * coverage) * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        });
        assert_eq!(&full.get_pixel(8, 9).0[..3], &expected);

        {
            let LayerNode::Group(outer) = tree.find_node_mut(outer_id).unwrap() else {
                unreachable!();
            };
            let LayerNode::Group(inner) = &mut outer.children[0] else {
                unreachable!();
            };
            inner
                .properties
                .disabled_channels
                .insert(Channel::BaseColor);
        }
        let disabled = tree.composite(64, Channel::BaseColor);
        assert_eq!(disabled.get_pixel(8, 9).0, Channel::BaseColor.background());
        {
            let LayerNode::Group(outer) = tree.find_node_mut(outer_id).unwrap() else {
                unreachable!();
            };
            let LayerNode::Group(inner) = &mut outer.children[0] else {
                unreachable!();
            };
            inner
                .properties
                .disabled_channels
                .remove(&Channel::BaseColor);
        }
        let normal = tree.composite(64, Channel::Normal);
        let normal_region = tree.composite_region(64, Channel::Normal, 8, 9, 1, 1);
        assert_eq!(normal_region.get_pixel(0, 0), normal.get_pixel(8, 9));
        let encoded = normal.get_pixel(8, 9).0;
        let decoded = decode_normal(&encoded[..3]);
        let length =
            (decoded[0] * decoded[0] + decoded[1] * decoded[1] + decoded[2] * decoded[2]).sqrt();
        assert!((length - 1.0).abs() < 0.01);
    }

    #[test]
    fn recursive_fill_blends_apply_multiply_add_and_detail_normal() {
        fn multiply(src: f32, dst: f32) -> f32 {
            src * dst
        }
        fn add(src: f32, dst: f32) -> f32 {
            (src + dst).min(1.0)
        }

        let mut tree = flat_tree();
        tree.roots.truncate(1);
        tree.selected_node_id = 1;
        let mut group = tree.allocate_group("Blends", 64).unwrap();
        group.properties.mask.put_pixel(10, 12, Luma([255]));
        let group_id = group.properties.id;
        let mut backdrop = Layer::new(tree.next_node_id, "Backdrop", 64);
        backdrop
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(10, 12, Rgba([50, 150, 220, 255]));
        backdrop
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(10, 12, Rgba([128, 128, 255, 255]));
        group.children.push(LayerNode::Paint(backdrop));
        let mut values = HashMap::new();
        values.insert(Channel::BaseColor, [128, 90, 220, 255]);
        values.insert(Channel::Normal, [190, 130, 220, 255]);
        let fill = FillLayer {
            properties: LayerNodeProperties::new(tree.next_node_id + 1, "Blend fill", 64).unwrap(),
            values,
        };
        group.children.push(LayerNode::Fill(fill));
        tree.insert(None, 1, LayerNode::Group(group)).unwrap();

        let background = [50_u8, 150, 220];
        for (mode, blend) in [
            (BlendMode::Multiply, multiply as fn(f32, f32) -> f32),
            (BlendMode::Add, add as fn(f32, f32) -> f32),
        ] {
            let LayerNode::Group(group) = tree.find_node_mut(group_id).unwrap() else {
                unreachable!();
            };
            let LayerNode::Fill(fill) = &mut group.children[1] else {
                unreachable!();
            };
            fill.properties.blend = mode;
            let source = fill.values[&Channel::BaseColor];
            let actual = tree.composite(64, Channel::BaseColor).get_pixel(10, 12).0;
            for index in 0..3 {
                let src = srgb_to_linear(source[index] as f32 / 255.0);
                let dst = srgb_to_linear(background[index] as f32 / 255.0);
                let expected = (linear_to_srgb(blend(src, dst)) * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                assert_eq!(actual[index], expected, "{mode:?}, component {index}");
            }
        }

        let LayerNode::Group(group) = tree.find_node_mut(group_id).unwrap() else {
            unreachable!();
        };
        let LayerNode::Fill(fill) = &mut group.children[1] else {
            unreachable!();
        };
        fill.properties.blend = BlendMode::Detail;
        let normal = tree.composite(64, Channel::Normal).get_pixel(10, 12).0;
        let decoded = decode_normal(&normal[..3]);
        let length =
            (decoded[0] * decoded[0] + decoded[1] * decoded[1] + decoded[2] * decoded[2]).sqrt();
        assert!((length - 1.0).abs() < 0.01);
        assert_ne!(normal, Channel::Normal.background());
    }

    #[test]
    fn expanded_blends_reach_recursive_compositor_with_channel_semantics() {
        let mut color = CompositePixel {
            value: [128, 128, 128, 255],
            coverage: 1.0,
        };
        blend_composite_pixel(
            &mut color,
            [128, 128, 128, 255],
            1.0,
            Channel::BaseColor,
            BlendMode::Screen,
        );
        let linear = srgb_to_linear(128.0 / 255.0);
        let expected = (linear_to_srgb(linear + linear - linear * linear) * 255.0).round() as u8;
        assert_eq!(color.value, [expected, expected, expected, 255]);

        let mut data = CompositePixel {
            value: [128, 128, 128, 255],
            coverage: 1.0,
        };
        blend_composite_pixel(
            &mut data,
            [64, 64, 64, 255],
            1.0,
            Channel::Roughness,
            BlendMode::Divide,
        );
        assert_eq!(data.value, [255, 255, 255, 255]);
    }

    #[test]
    fn validation_rejects_duplicate_ids_and_depth_overflow() {
        let mut duplicate = flat_tree();
        duplicate
            .roots
            .push(LayerNode::Paint(Layer::new(1, "Duplicate", 64)));
        assert!(duplicate.validate(64).is_err());

        let mut deep = LayerTree {
            roots: vec![LayerNode::Paint(Layer::new(1, "Deep leaf", 64))],
            selected_node_id: 1,
            next_node_id: MAX_LAYER_TREE_DEPTH as u32 + 2,
            extensions: JsonMap::new(),
        };
        for id in (2..=MAX_LAYER_TREE_DEPTH as u32 + 1).rev() {
            deep.roots = vec![LayerNode::Group(LayerGroup {
                properties: LayerNodeProperties::new(id, format!("Group {id}"), 64).unwrap(),
                children: deep.roots,
            })];
        }
        assert!(deep.validate(64).is_err());
    }

    #[test]
    fn validation_rejects_node_bound_and_empty_fill_values() {
        let many = LayerTree {
            roots: (1..=MAX_LAYER_TREE_NODES as u32 + 1)
                .map(|id| LayerNode::Paint(Layer::new(id, format!("Layer {id}"), 64)))
                .collect(),
            selected_node_id: 1,
            next_node_id: MAX_LAYER_TREE_NODES as u32 + 2,
            extensions: JsonMap::new(),
        };
        assert!(many.validate(64).is_err());

        let mut empty_fill = flat_tree();
        let fill = empty_fill.allocate_fill("Empty", 64, HashMap::new());
        assert!(fill.is_err());
        assert_eq!(many.next_node_id, MAX_LAYER_TREE_NODES as u32 + 2);
    }

    #[test]
    fn constructors_allocate_fresh_ids_and_keep_fill_values() {
        let mut tree = flat_tree();
        let values = HashMap::from([(Channel::BaseColor, [10, 20, 30, 255])]);
        let fill = tree.allocate_fill("Paint", 64, values.clone()).unwrap();
        assert_eq!(fill.properties.id, 3);
        assert_eq!(fill.values, values);
        tree.roots.push(LayerNode::Fill(fill));
        let group = tree.allocate_group("Folder", 64).unwrap();
        assert_eq!(group.properties.id, 4);
        assert!(group.children.is_empty());
        assert_eq!(tree.next_node_id, 5);
        tree.roots.push(LayerNode::Group(group));
        tree.validate(64).unwrap();
    }

    #[test]
    fn isolated_nested_fill_keeps_ancestor_context_and_excludes_siblings() {
        let mut tree = flat_tree();
        tree.roots.truncate(1);
        tree.selected_node_id = 1;
        let mut group = tree.allocate_group("Masked group", 64).unwrap();
        group.properties.opacity = 0.5;
        group.properties.mask.put_pixel(8, 9, Luma([128]));
        group.properties.mask.put_pixel(10, 11, Luma([0]));
        let group_id = group.properties.id;

        let mut target = tree
            .allocate_fill(
                "Target",
                64,
                HashMap::from([(Channel::BaseColor, [240, 20, 30, 255])]),
            )
            .unwrap();
        target.properties.opacity = 0.75;
        target.properties.mask.put_pixel(8, 9, Luma([192]));
        let target_id = target.properties.id;
        let sibling = tree
            .allocate_fill(
                "Sibling",
                64,
                HashMap::from([(Channel::BaseColor, [10, 230, 40, 255])]),
            )
            .unwrap();
        group.children.push(LayerNode::Fill(target));
        group.children.push(LayerNode::Fill(sibling));
        tree.insert(None, 1, LayerNode::Group(group)).unwrap();
        tree.select(target_id).unwrap();

        let visible_before = tree
            .all_node_ids()
            .into_iter()
            .map(|id| {
                let visible = match tree.find_node(id).unwrap() {
                    LayerNode::Paint(layer) => layer.visible,
                    LayerNode::Fill(fill) => fill.properties.visible,
                    LayerNode::Group(group) => group.properties.visible,
                };
                (id, visible)
            })
            .collect::<Vec<_>>();
        let isolated = tree
            .composite_isolated(target_id, 64, Channel::BaseColor)
            .expect("target should be compositable");
        let region = tree
            .composite_isolated_region(target_id, 64, Channel::BaseColor, 6, 7, 7, 6)
            .unwrap();
        for local_y in 0..6 {
            for local_x in 0..7 {
                assert_eq!(
                    region.get_pixel(local_x, local_y),
                    isolated.get_pixel(local_x + 6, local_y + 7)
                );
            }
        }
        assert_eq!(
            isolated.get_pixel(10, 11).0,
            Channel::BaseColor.background()
        );
        let full = tree.composite(64, Channel::BaseColor);
        assert_ne!(isolated.get_pixel(8, 9), full.get_pixel(8, 9));
        assert_eq!(
            visible_before,
            tree.all_node_ids()
                .into_iter()
                .map(|id| {
                    let visible = match tree.find_node(id).unwrap() {
                        LayerNode::Paint(layer) => layer.visible,
                        LayerNode::Fill(fill) => fill.properties.visible,
                        LayerNode::Group(group) => group.properties.visible,
                    };
                    (id, visible)
                })
                .collect::<Vec<_>>()
        );

        let LayerNode::Group(group) = tree.find_node_mut(group_id).unwrap() else {
            panic!("expected Group");
        };
        group
            .properties
            .disabled_channels
            .insert(Channel::BaseColor);
        let disabled = tree
            .composite_isolated(target_id, 64, Channel::BaseColor)
            .unwrap();
        assert_eq!(disabled.get_pixel(8, 9).0, Channel::BaseColor.background());
    }
}

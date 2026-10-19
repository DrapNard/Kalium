//! Bounded, sparse undo/redo history. Interactive strokes retain only changed
//! rows from touched 64×64 tiles; fill/clear snapshots are split into chunks.
use crate::document::{BlendMode, Channel, Document, Layer, MAX_LAYERS};
use crate::layer_tree::{FillLayer, LayerGroup, LayerNode, LayerNodeProperties, LayerTree};
use crate::mesh::Mesh;
use crate::paint::BrushMode;
use crate::procedural::ProceduralGraph;
use crate::recovery::{DocumentEvent, LayerImageTarget, LayerSnapshotMetadata};
use image::{GrayImage, RgbaImage};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub const MAX_HISTORY: usize = 64;
pub const MAX_HISTORY_BYTES: usize = 96 * 1024 * 1024;
const CHUNK_BYTES: usize = 4096;
pub const STROKE_TILE_SIZE: u32 = 64;
const MAX_RECOVERY_EVENT_DATA_BYTES: usize = 1024 * 1024;
const MAX_RECOVERY_EVENT_COUNT: usize = 16_384;
const RECOVERY_LAYER_CHUNK_BYTES: usize = 256 * 1024;
const MAX_TREE_EXTENSION_STORAGE_BYTES: usize = 48 * 1024 * 1024;

fn stroke_image_bytes(node: &LayerNode, mask: bool, channel: Channel) -> Option<&[u8]> {
    if mask {
        Some(node.mask().as_raw())
    } else if let LayerNode::Paint(layer) = node {
        layer
            .pixels
            .get(&channel)
            .map(|image| image.as_raw().as_slice())
    } else {
        None
    }
}

fn stroke_image_bytes_mut(
    doc: &mut Document,
    layer_id: u32,
    mask: bool,
    channel: Channel,
    size: u32,
) -> Option<&mut [u8]> {
    if mask {
        Some(doc.layer_tree.find_node_mut(layer_id)?.mask_mut().as_mut())
    } else {
        Some(
            doc.paint_layer_mut(layer_id)?
                .pixels_mut(channel, size)
                .as_mut(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryDirection {
    Undo,
    Redo,
}

impl HistoryDirection {
    fn is_undo(self) -> bool {
        matches!(self, Self::Undo)
    }
}

pub struct Snapshot {
    pub layer_id: u32,
    pub channel: Channel,
    pub mask: bool,
    pub channel_was_present: bool,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

/// Sparse before-image capture for one interactive brush stroke.
pub struct StrokeCapture {
    layer_id: u32,
    channel: Channel,
    mask: bool,
    channel_was_present: bool,
    size: u32,
    tiles: HashMap<usize, TileBefore>,
    reserved_bytes: usize,
    over_limit: bool,
}

impl StrokeCapture {
    #[cfg(test)]
    pub fn projected_storage_bytes(&self) -> usize {
        self.reserved_bytes
    }
}

fn collect_stroke_after_images(
    capture: &StrokeCapture,
    doc: &Document,
    bytes_per_pixel: usize,
    stride: usize,
) -> Option<Vec<(usize, Vec<u8>)>> {
    if capture.over_limit
        || capture.size != doc.size
        || stride != capture.size as usize * bytes_per_pixel
    {
        return None;
    }
    let node = doc.layer_tree.find_node(capture.layer_id)?;
    let source = stroke_image_bytes(node, capture.mask, capture.channel);
    if source.is_some_and(|source| source.len() != stride.saturating_mul(capture.size as usize)) {
        return None;
    }
    let mut after_tiles = Vec::new();
    after_tiles.try_reserve_exact(capture.tiles.len()).ok()?;
    for (&key, tile) in &capture.tiles {
        if tile.width == 0
            || tile.height == 0
            || tile
                .x
                .checked_add(tile.width)
                .is_none_or(|right| right > capture.size)
            || tile
                .y
                .checked_add(tile.height)
                .is_none_or(|bottom| bottom > capture.size)
        {
            return None;
        }
        let row_bytes = tile.width as usize * bytes_per_pixel;
        let payload_bytes = row_bytes.saturating_mul(tile.height as usize);
        if tile.before.len() != payload_bytes {
            return None;
        }
        let mut after = Vec::new();
        after.try_reserve_exact(payload_bytes).ok()?;
        if let Some(source) = source {
            for row in tile.y..tile.y + tile.height {
                let start = row as usize * stride + tile.x as usize * bytes_per_pixel;
                after.extend_from_slice(&source[start..start + row_bytes]);
            }
        } else {
            after.resize(payload_bytes, 0);
        }
        if tile.before != after {
            after_tiles.push((key, after));
        }
    }
    Some(after_tiles)
}

impl History {
    /// Build changed-tile after-images for a completed fill, clear, or mask
    /// operation. The source snapshot must describe the current active set.
    pub(crate) fn snapshot_recovery_events(
        snapshot: &Snapshot,
        doc: &Document,
        set_id: u32,
    ) -> Option<Vec<DocumentEvent>> {
        if set_id != doc.active_set_id {
            return None;
        }
        let bytes_per_pixel = if snapshot.mask { 1usize } else { 4usize };
        let expected_len = (doc.size as usize)
            .checked_mul(doc.size as usize)?
            .checked_mul(bytes_per_pixel)?;
        if snapshot.before.len() != expected_len || snapshot.after.len() != expected_len {
            return None;
        }
        let node = doc.layer_tree.find_node(snapshot.layer_id)?;
        let current = stroke_image_bytes(node, snapshot.mask, snapshot.channel);
        if current.is_some_and(|pixels| pixels.len() != expected_len)
            || current.is_some_and(|pixels| pixels != snapshot.after)
            || current.is_none() && snapshot.after.iter().any(|byte| *byte != 0)
        {
            return None;
        }

        let columns = doc.size.div_ceil(STROKE_TILE_SIZE);
        let tile_count = columns.checked_mul(columns)? as usize;
        let mut events = Vec::new();
        events.try_reserve_exact(tile_count).ok()?;
        let target = if snapshot.mask {
            LayerImageTarget::Mask
        } else {
            LayerImageTarget::Channel(snapshot.channel)
        };
        let stride = doc.size as usize * bytes_per_pixel;
        let mut event_data_bytes = 0usize;
        for tile_y in 0..columns {
            for tile_x in 0..columns {
                let x = tile_x * STROKE_TILE_SIZE;
                let y = tile_y * STROKE_TILE_SIZE;
                let width = STROKE_TILE_SIZE.min(doc.size - x);
                let height = STROKE_TILE_SIZE.min(doc.size - y);
                let row_bytes = width as usize * bytes_per_pixel;
                let tile_bytes = row_bytes.checked_mul(height as usize)?;
                let mut changed = false;
                for row in y..y + height {
                    let start = row as usize * stride + x as usize * bytes_per_pixel;
                    let end = start + row_bytes;
                    if snapshot.before[start..end] != snapshot.after[start..end] {
                        changed = true;
                    }
                }
                if changed {
                    let projected = event_data_bytes.checked_add(tile_bytes)?;
                    if projected > MAX_RECOVERY_EVENT_DATA_BYTES {
                        return None;
                    }
                    let mut after = Vec::new();
                    after.try_reserve_exact(tile_bytes).ok()?;
                    for row in y..y + height {
                        let start = row as usize * stride + x as usize * bytes_per_pixel;
                        after.extend_from_slice(&snapshot.after[start..start + row_bytes]);
                    }
                    events.push(DocumentEvent::LayerImagePatch {
                        set_id,
                        layer_id: snapshot.layer_id,
                        target,
                        x,
                        y,
                        width,
                        height,
                        bytes: after,
                    });
                    event_data_bytes = projected;
                }
            }
        }
        Some(events)
    }

    /// Build forward recovery events for the changed tiles in an active stroke.
    /// The capture stays live so the caller can still commit it to undo history.
    pub(crate) fn stroke_recovery_events(
        capture: &StrokeCapture,
        doc: &Document,
        set_id: u32,
    ) -> Option<Vec<DocumentEvent>> {
        if capture.over_limit || capture.size != doc.size || set_id != doc.active_set_id {
            return None;
        }
        let bytes_per_pixel = if capture.mask { 1 } else { 4 };
        let projected_data = capture.tiles.values().try_fold(0usize, |sum, tile| {
            let tile_bytes = (tile.width as usize)
                .checked_mul(tile.height as usize)?
                .checked_mul(bytes_per_pixel)?;
            sum.checked_add(tile_bytes)
        })?;
        if projected_data > MAX_RECOVERY_EVENT_DATA_BYTES {
            return None;
        }
        let stride = capture.size as usize * bytes_per_pixel;
        let after_tiles = collect_stroke_after_images(capture, doc, bytes_per_pixel, stride)?;
        let mut events = Vec::new();
        events.try_reserve_exact(after_tiles.len()).ok()?;
        let target = if capture.mask {
            LayerImageTarget::Mask
        } else {
            LayerImageTarget::Channel(capture.channel)
        };
        for (key, bytes) in after_tiles {
            let tile = capture.tiles.get(&key)?;
            events.push(DocumentEvent::LayerImagePatch {
                set_id,
                layer_id: capture.layer_id,
                target,
                x: tile.x,
                y: tile.y,
                width: tile.width,
                height: tile.height,
                bytes,
            });
        }
        Some(events)
    }

    /// Prepare forward recovery events that reproduce the next undo or redo.
    ///
    /// This method only reads the history and document. The caller can encode
    /// and durably append the returned events before invoking `undo` or `redo`.
    /// The final event always restores the selection that the history operation
    /// will leave active. Retained layer payloads are emitted as bounded,
    /// transaction-marked snapshot chunks.
    #[cfg(test)]
    pub fn prepare_recovery_events(
        &self,
        doc: &Document,
        set_id: u32,
        direction: HistoryDirection,
    ) -> Option<Vec<DocumentEvent>> {
        let mut events = Vec::new();
        self.visit_recovery_events(doc, set_id, direction, |event| {
            events.push(event);
            true
        })?;
        Some(events)
    }

    /// Visit recovery events after validating the entire plan. Retained-layer
    /// pixels are copied into one bounded chunk at a time and released as soon
    /// as the sink returns. A sink returning `false` aborts the visit.
    pub fn visit_recovery_events(
        &self,
        doc: &Document,
        set_id: u32,
        direction: HistoryDirection,
        mut sink: impl FnMut(DocumentEvent) -> bool,
    ) -> Option<()> {
        if set_id == 0
            || set_id != doc.active_set_id
            || doc
                .layer_tree
                .find_node(doc.layer_tree.selected_node_id)
                .is_none()
        {
            return None;
        }
        let undo = direction.is_undo();
        let entry = if undo {
            self.undo.last()?
        } else {
            self.redo.last()?
        };
        let mut events = Vec::new();
        let mut event_data_bytes = 0usize;
        let mut final_selection = doc.layer_tree.selected_node_id;
        let mut retained_snapshot: Option<LayerSnapshotVisit<'_>> = None;
        let mut snapshot_event_index = None;

        match entry {
            Entry::Pixels(delta) => {
                if delta.patches.is_empty() {
                    return None;
                }
                let node = doc.layer_tree.find_node(delta.layer_id)?;
                let bytes_per_pixel = if delta.mask { 1usize } else { 4usize };
                let row_stride = (doc.size as usize).checked_mul(bytes_per_pixel)?;
                if row_stride == 0 || doc.size == 0 {
                    return None;
                }
                let image_len = row_stride.checked_mul(doc.size as usize)?;
                let current = stroke_image_bytes(node, delta.mask, delta.channel);
                if delta.mask && current.is_none_or(|bytes| bytes.len() != image_len) {
                    return None;
                }
                if current.is_some_and(|bytes| bytes.len() != image_len) {
                    return None;
                }
                if !delta.mask && delta.channel_was_present && current.is_none() {
                    return None;
                }
                if !delta.mask && !delta.channel_was_present {
                    // Undo starts from the painted channel; Redo starts from
                    // the absent channel left by that undo.
                    if undo && current.is_none() || !undo && current.is_some() {
                        return None;
                    }
                }

                let mut remove_absent_channel = false;
                for patch in &delta.patches {
                    if patch.rows == 0
                        || patch.row_bytes == 0
                        || patch.row_bytes % bytes_per_pixel != 0
                        || patch.stride < patch.row_bytes
                        || patch.before.len() != patch.after.len()
                        || patch.row_bytes.checked_mul(patch.rows)? != patch.before.len()
                    {
                        return None;
                    }
                    let source = if undo { &patch.after } else { &patch.before };
                    let target = if undo { &patch.before } else { &patch.after };
                    for row in 0..patch.rows {
                        let row_start = patch.offset.checked_add(row.checked_mul(patch.stride)?)?;
                        let row_end = row_start.checked_add(patch.row_bytes)?;
                        if row_end > image_len {
                            return None;
                        }
                        let source_start = row.checked_mul(patch.row_bytes)?;
                        let source_row =
                            source.get(source_start..source_start.checked_add(patch.row_bytes)?)?;
                        let target_row =
                            target.get(source_start..source_start.checked_add(patch.row_bytes)?)?;
                        let mut consumed = 0usize;
                        while consumed < patch.row_bytes {
                            let destination = row_start.checked_add(consumed)?;
                            let column_bytes = destination % row_stride;
                            let segment_len = (patch.row_bytes - consumed)
                                .min(row_stride.checked_sub(column_bytes)?);
                            if column_bytes % bytes_per_pixel != 0
                                || segment_len == 0
                                || segment_len % bytes_per_pixel != 0
                            {
                                return None;
                            }
                            if let Some(current) = current {
                                let end = destination.checked_add(segment_len)?;
                                if current.get(destination..end)?
                                    != source_row
                                        .get(consumed..consumed.checked_add(segment_len)?)?
                                {
                                    return None;
                                }
                            } else if !source_row
                                .get(consumed..consumed.checked_add(segment_len)?)?
                                .iter()
                                .all(|byte| *byte == 0)
                            {
                                // A channel absent before its first paint is
                                // logically all-zero on the pre-edit side.
                                return None;
                            }

                            if !delta.mask && !delta.channel_was_present && undo {
                                remove_absent_channel = true;
                            } else {
                                event_data_bytes = event_data_bytes.checked_add(segment_len)?;
                                if event_data_bytes > MAX_RECOVERY_EVENT_DATA_BYTES {
                                    return None;
                                }
                                let y = u32::try_from(destination / row_stride).ok()?;
                                let x = u32::try_from(column_bytes / bytes_per_pixel).ok()?;
                                let width = u32::try_from(segment_len / bytes_per_pixel).ok()?;
                                let height = 1;
                                let mut bytes = Vec::new();
                                bytes.try_reserve_exact(segment_len).ok()?;
                                bytes.extend_from_slice(
                                    target_row.get(consumed..consumed.checked_add(segment_len)?)?,
                                );
                                if !push_bounded_event(
                                    &mut events,
                                    DocumentEvent::LayerImagePatch {
                                        set_id,
                                        layer_id: delta.layer_id,
                                        target: if delta.mask {
                                            LayerImageTarget::Mask
                                        } else {
                                            LayerImageTarget::Channel(delta.channel)
                                        },
                                        x,
                                        y,
                                        width,
                                        height,
                                        bytes,
                                    },
                                ) {
                                    return None;
                                }
                            }
                            consumed = consumed.checked_add(segment_len)?;
                        }
                    }
                }
                if remove_absent_channel
                    && !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerChannelRemove {
                            set_id,
                            layer_id: delta.layer_id,
                            channel: delta.channel,
                        },
                    )
                {
                    return None;
                }
            }
            Entry::Properties(delta) => {
                let node = doc.layer_tree.find_node(delta.layer_id)?;
                let current = LayerProperties::capture_node(node);
                let (source, target) = if undo {
                    (&delta.after, &delta.before)
                } else {
                    (&delta.before, &delta.after)
                };
                if &current != source {
                    return None;
                }
                if !push_bounded_event(
                    &mut events,
                    DocumentEvent::LayerTreeNodePropertiesSet {
                        set_id,
                        node_id: delta.layer_id,
                        name: target.name.clone(),
                        visible: target.visible,
                        opacity: target.opacity,
                        blend: target.blend,
                        disabled_channels: target.disabled_channels.clone(),
                        procedural_graph: target.procedural_graph.clone(),
                    },
                ) {
                    return None;
                }
            }
            Entry::FillValue(delta) => {
                let LayerNode::Fill(fill) = doc.layer_tree.find_node(delta.node_id)? else {
                    return None;
                };
                let (source, target) = if undo {
                    (delta.after, delta.before)
                } else {
                    (delta.before, delta.after)
                };
                if fill.values.get(&delta.channel).copied() != source
                    || !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerTreeFillValueSet {
                            set_id,
                            node_id: delta.node_id,
                            channel: delta.channel,
                            value: target,
                        },
                    )
                {
                    return None;
                }
            }
            Entry::TreeStructure(delta) => {
                let current_selection = doc.layer_tree.selected_node_id;
                match (&delta.change, undo) {
                    (TreeStructureChange::Inserted { .. }, true) => {
                        if current_selection != delta.selected_after_id
                            || doc.layer_tree.parent_id(delta.node_id) != Some(delta.parent_id)
                            || doc
                                .layer_tree
                                .siblings(delta.parent_id)
                                .ok()
                                .and_then(|nodes| nodes.get(delta.index))
                                .map(LayerNode::id)
                                != Some(delta.node_id)
                            || doc.layer_tree.find_node(delta.selected_before_id).is_none()
                            || tree_extension_storage_bytes(&doc.layer_tree)
                                > MAX_TREE_EXTENSION_STORAGE_BYTES
                        {
                            return None;
                        }
                        final_selection = delta.selected_before_id;
                        let expected_index = u32::try_from(delta.index).ok()?;
                        if !push_bounded_event(
                            &mut events,
                            DocumentEvent::LayerTreeNodeRemove {
                                set_id,
                                node_id: delta.node_id,
                                expected_parent_id: delta.parent_id,
                                expected_index,
                                selected_after_node_id: final_selection,
                            },
                        ) {
                            return None;
                        }
                    }
                    (TreeStructureChange::Inserted { payload }, false) => {
                        if current_selection != delta.selected_before_id
                            || doc.layer_tree.find_node(delta.node_id).is_some()
                        {
                            return None;
                        }
                        let node = payload.as_ref()?;
                        if node.id() != delta.node_id
                            || node_storage_bytes(node) > MAX_HISTORY_BYTES
                            || tree_extension_storage_bytes(&doc.layer_tree)
                                .saturating_add(subtree_extension_storage_bytes(node))
                                > MAX_TREE_EXTENSION_STORAGE_BYTES
                            || delta.index > doc.layer_tree.siblings(delta.parent_id).ok()?.len()
                        {
                            return None;
                        }
                        final_selection = delta.selected_after_id;
                        for event in crate::recovery::prepare_layer_tree_subtree_events(
                            node,
                            set_id,
                            delta.parent_id,
                            u32::try_from(delta.index).ok()?,
                            final_selection,
                        )
                        .ok()?
                        {
                            if !push_bounded_event(&mut events, event) {
                                return None;
                            }
                        }
                    }
                    (TreeStructureChange::Removed { payload }, true) => {
                        if current_selection != delta.selected_after_id
                            || doc.layer_tree.find_node(delta.node_id).is_some()
                            || delta.index > doc.layer_tree.siblings(delta.parent_id).ok()?.len()
                            || tree_extension_storage_bytes(&doc.layer_tree)
                                .saturating_add(subtree_extension_storage_bytes(payload.as_ref()?))
                                > MAX_TREE_EXTENSION_STORAGE_BYTES
                        {
                            return None;
                        }
                        let node = payload.as_ref()?;
                        if node.id() != delta.node_id
                            || (!subtree_contains_id(node, delta.selected_before_id)
                                && doc.layer_tree.find_node(delta.selected_before_id).is_none())
                            || node_storage_bytes(node) > MAX_HISTORY_BYTES
                        {
                            return None;
                        }
                        final_selection = delta.selected_before_id;
                        for event in crate::recovery::prepare_layer_tree_subtree_events(
                            node,
                            set_id,
                            delta.parent_id,
                            u32::try_from(delta.index).ok()?,
                            final_selection,
                        )
                        .ok()?
                        {
                            if !push_bounded_event(&mut events, event) {
                                return None;
                            }
                        }
                    }
                    (TreeStructureChange::Removed { .. }, false) => {
                        if current_selection != delta.selected_before_id
                            || doc.layer_tree.parent_id(delta.node_id) != Some(delta.parent_id)
                            || doc
                                .layer_tree
                                .siblings(delta.parent_id)
                                .ok()
                                .and_then(|nodes| nodes.get(delta.index))
                                .map(LayerNode::id)
                                != Some(delta.node_id)
                            || doc.layer_tree.find_node(delta.selected_after_id).is_none()
                            || tree_extension_storage_bytes(&doc.layer_tree)
                                > MAX_TREE_EXTENSION_STORAGE_BYTES
                        {
                            return None;
                        }
                        final_selection = delta.selected_after_id;
                        if !push_bounded_event(
                            &mut events,
                            DocumentEvent::LayerTreeNodeRemove {
                                set_id,
                                node_id: delta.node_id,
                                expected_parent_id: delta.parent_id,
                                expected_index: u32::try_from(delta.index).ok()?,
                                selected_after_node_id: final_selection,
                            },
                        ) {
                            return None;
                        }
                    }
                    (
                        TreeStructureChange::Moved {
                            from_parent_id,
                            from_index,
                            to_parent_id,
                            to_index,
                        },
                        _,
                    ) => {
                        let (source_parent, source_index, target_parent, target_index) = if undo {
                            (*to_parent_id, *to_index, *from_parent_id, *from_index)
                        } else {
                            (*from_parent_id, *from_index, *to_parent_id, *to_index)
                        };
                        if current_selection != delta.selected_before_id
                            || doc.layer_tree.parent_id(delta.node_id) != Some(source_parent)
                            || doc
                                .layer_tree
                                .siblings(source_parent)
                                .ok()
                                .and_then(|nodes| nodes.get(source_index))
                                .map(LayerNode::id)
                                != Some(delta.node_id)
                            || target_index
                                > doc
                                    .layer_tree
                                    .siblings(target_parent)
                                    .ok()?
                                    .len()
                                    .saturating_sub(usize::from(source_parent == target_parent))
                            || tree_extension_storage_bytes(&doc.layer_tree)
                                > MAX_TREE_EXTENSION_STORAGE_BYTES
                        {
                            return None;
                        }
                        if !push_bounded_event(
                            &mut events,
                            DocumentEvent::LayerTreeNodeMove {
                                set_id,
                                node_id: delta.node_id,
                                from_parent_id: source_parent,
                                from_index: u32::try_from(source_index).ok()?,
                                to_parent_id: target_parent,
                                to_index: u32::try_from(target_index).ok()?,
                            },
                        ) {
                            return None;
                        }
                    }
                }
            }
            Entry::Uniform(delta) => {
                if delta.inactive_present != delta.inactive.is_some()
                    || delta.inactive_is_after == undo
                {
                    return None;
                }
                let layer = doc.paint_layer(delta.layer_id)?;
                let (target, current): (LayerImageTarget, Option<&[u8]>) = match delta.target {
                    UniformTarget::Channel(channel) => (
                        LayerImageTarget::Channel(channel),
                        layer
                            .pixels
                            .get(&channel)
                            .filter(|image| image.dimensions() == (doc.size, doc.size))
                            .map(|image| image.as_raw().as_slice()),
                    ),
                    UniformTarget::Mask => {
                        if layer.mask.dimensions() != (doc.size, doc.size) {
                            return None;
                        }
                        (LayerImageTarget::Mask, Some(layer.mask.as_raw()))
                    }
                };
                if undo {
                    if !current.is_some_and(|bytes| uniform_value_matches(bytes, delta.value)) {
                        return None;
                    }
                    if let Some(before) = delta.inactive.as_deref() {
                        let layer_index = doc
                            .layer_tree
                            .path_to(layer.id)
                            .and_then(|path| path.last().copied())?;
                        let (begin, chunk_count) = make_layer_snapshot_begin(
                            layer,
                            doc.size,
                            set_id,
                            layer_index,
                            final_selection,
                            Some((target, Some(before))),
                        )?;
                        if snapshot_event_index.is_some() {
                            return None;
                        }
                        snapshot_event_index = Some(events.len());
                        retained_snapshot = Some(LayerSnapshotVisit {
                            layer,
                            selected_after_id: final_selection,
                            chunk_count,
                            override_image: Some((target, Some(before))),
                        });
                        if !push_bounded_event(&mut events, begin) {
                            return None;
                        }
                    } else if let UniformTarget::Channel(channel) = delta.target {
                        if !push_bounded_event(
                            &mut events,
                            DocumentEvent::LayerChannelRemove {
                                set_id,
                                layer_id: delta.layer_id,
                                channel,
                            },
                        ) {
                            return None;
                        }
                    } else {
                        return None;
                    }
                } else {
                    if current.is_some() != delta.before_present
                        || uniform_state_hash(current) != delta.before_hash
                    {
                        return None;
                    }
                    let event = match (delta.target, delta.value) {
                        (UniformTarget::Channel(channel), UniformValue::Rgba(value)) => {
                            DocumentEvent::LayerImageFill {
                                set_id,
                                layer_id: delta.layer_id,
                                channel,
                                value,
                            }
                        }
                        (UniformTarget::Mask, UniformValue::Gray(value)) => {
                            DocumentEvent::LayerMaskSet {
                                set_id,
                                layer_id: delta.layer_id,
                                value,
                            }
                        }
                        _ => return None,
                    };
                    if !push_bounded_event(&mut events, event) {
                        return None;
                    }
                }
            }
            Entry::MaskInvert { layer_id } => {
                let node = doc.layer_tree.find_node(*layer_id)?;
                let mask = match node {
                    LayerNode::Paint(layer) => &layer.mask,
                    LayerNode::Fill(fill) => &fill.properties.mask,
                    LayerNode::Group(group) => &group.properties.mask,
                };
                if mask.dimensions() != (doc.size, doc.size)
                    || !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerMaskInvert {
                            set_id,
                            layer_id: *layer_id,
                        },
                    )
                {
                    return None;
                }
            }
            Entry::Stack(delta) => match (&delta.change, undo) {
                (
                    StackChange::Moved {
                        old_index,
                        new_index,
                    },
                    _,
                ) => {
                    let expected_index = if undo { *new_index } else { *old_index };
                    let target_index = if undo { *old_index } else { *new_index };
                    let expected_selection = if undo {
                        delta.selected_after
                    } else {
                        delta.selected_before
                    };
                    final_selection = if undo {
                        delta.selected_before
                    } else {
                        delta.selected_after
                    };
                    if doc
                        .layer_tree
                        .roots
                        .iter()
                        .position(|node| node.id() == delta.layer_id)
                        != Some(expected_index)
                        || doc.layer_tree.selected_node_id != expected_selection
                    {
                        return None;
                    }
                    if !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerReorder {
                            set_id,
                            layer_id: delta.layer_id,
                            to_index: target_index,
                        },
                    ) {
                        return None;
                    }
                }
                (StackChange::Added { .. }, true) => {
                    final_selection = delta.selected_before;
                    if doc.layer_tree.parent_id(delta.layer_id) != Some(None)
                        || doc.layer_tree.find_node(final_selection).is_none()
                    {
                        return None;
                    }
                    if !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerSelection {
                            set_id,
                            layer_id: delta.layer_id,
                        },
                    ) || !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerRemove {
                            set_id,
                            layer_id: delta.layer_id,
                            selected_after_id: final_selection,
                        },
                    ) {
                        return None;
                    }
                }
                (
                    StackChange::Added {
                        name,
                        duplicate_source,
                        payload,
                    },
                    false,
                ) => {
                    if doc.layer_tree.find_node(delta.layer_id).is_some()
                        || delta.index > doc.layer_tree.roots.len()
                        || doc.layer_tree.selected_node_id != delta.selected_before
                    {
                        return None;
                    }
                    final_selection = delta.selected_after;
                    if let Some(layer) = payload.as_ref() {
                        if layer.id != delta.layer_id || snapshot_event_index.is_some() {
                            return None;
                        }
                        let (begin, chunk_count) = make_layer_snapshot_begin(
                            layer,
                            doc.size,
                            set_id,
                            delta.index,
                            final_selection,
                            None,
                        )?;
                        snapshot_event_index = Some(events.len());
                        retained_snapshot = Some(LayerSnapshotVisit {
                            layer,
                            selected_after_id: final_selection,
                            chunk_count,
                            override_image: None,
                        });
                        if !push_bounded_event(&mut events, begin) {
                            return None;
                        }
                    } else {
                        if final_selection != delta.layer_id
                            && doc.layer_tree.find_node(final_selection).is_none()
                        {
                            return None;
                        }
                        if let Some(source_layer_id) = duplicate_source {
                            doc.paint_layer(*source_layer_id)?;
                            if !push_bounded_event(
                                &mut events,
                                DocumentEvent::LayerReinsert {
                                    set_id,
                                    layer_id: delta.layer_id,
                                    to_index: delta.index,
                                    name: name.clone(),
                                    source_layer_id: Some(*source_layer_id),
                                },
                            ) {
                                return None;
                            }
                        } else if !push_bounded_event(
                            &mut events,
                            DocumentEvent::LayerReinsert {
                                set_id,
                                layer_id: delta.layer_id,
                                to_index: delta.index,
                                name: name.clone(),
                                source_layer_id: None,
                            },
                        ) {
                            return None;
                        }
                    }
                }
                (StackChange::Removed { .. }, false) => {
                    final_selection = delta.selected_after;
                    if doc.layer_tree.find_node(delta.layer_id).is_none()
                        || doc.layer_tree.find_node(final_selection).is_none()
                    {
                        return None;
                    }
                    if !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerSelection {
                            set_id,
                            layer_id: delta.layer_id,
                        },
                    ) || !push_bounded_event(
                        &mut events,
                        DocumentEvent::LayerRemove {
                            set_id,
                            layer_id: delta.layer_id,
                            selected_after_id: final_selection,
                        },
                    ) {
                        return None;
                    }
                }
                (StackChange::Removed { payload }, true) => {
                    let layer = payload.as_ref()?;
                    if layer.id != delta.layer_id
                        || doc.layer_tree.find_node(delta.layer_id).is_some()
                        || doc.layer_tree.selected_node_id != delta.selected_after
                        || delta.index > doc.layer_tree.roots.len()
                    {
                        return None;
                    }
                    final_selection = delta.selected_before;
                    if snapshot_event_index.is_some() {
                        return None;
                    }
                    let (begin, chunk_count) = make_layer_snapshot_begin(
                        layer,
                        doc.size,
                        set_id,
                        delta.index,
                        final_selection,
                        None,
                    )?;
                    snapshot_event_index = Some(events.len());
                    retained_snapshot = Some(LayerSnapshotVisit {
                        layer,
                        selected_after_id: final_selection,
                        chunk_count,
                        override_image: None,
                    });
                    if !push_bounded_event(&mut events, begin) {
                        return None;
                    }
                }
            },
            // Mesh replacements are journaled by the app as their own
            // MeshReplace transaction; they do not fit the layer-event plan.
            Entry::Mesh { .. } => return None,
        }

        let selection_will_be_reinserted = matches!(
            (entry, undo),
            (
                Entry::Stack(StackDelta {
                    layer_id,
                    change: StackChange::Added { .. },
                    ..
                }),
                false
            ) if *layer_id == final_selection
        ) || matches!(
            (entry, undo),
            (
                Entry::TreeStructure(TreeStructureDelta {
                    change: TreeStructureChange::Inserted { payload: Some(node) },
                    ..
                }),
                false
            ) if subtree_contains_id(node, final_selection)
        ) || matches!(
            (entry, undo),
            (
                Entry::TreeStructure(TreeStructureDelta {
                    change: TreeStructureChange::Removed { payload: Some(node) },
                    ..
                }),
                true
            ) if subtree_contains_id(node, final_selection)
        ) || matches!(
            (entry, undo),
            (
                Entry::Stack(StackDelta {
                    layer_id,
                    change: StackChange::Removed { payload: Some(_) },
                    ..
                }),
                true
            ) if *layer_id == final_selection
        );
        if event_data_bytes > MAX_RECOVERY_EVENT_DATA_BYTES
            || (!selection_will_be_reinserted
                && doc.layer_tree.find_node(final_selection).is_none())
        {
            return None;
        }
        if matches!(entry, Entry::Stack(_))
            && !push_bounded_event(
                &mut events,
                DocumentEvent::LayerSelection {
                    set_id,
                    layer_id: final_selection,
                },
            )
        {
            return None;
        }
        if let Some(snapshot) = &retained_snapshot {
            let chunk_count = snapshot.chunk_count;
            if events.len().checked_add(chunk_count)?.checked_add(1)? > MAX_RECOVERY_EVENT_COUNT {
                return None;
            }
        }

        let Some(snapshot_index) = snapshot_event_index else {
            for event in events {
                if !sink(event) {
                    return None;
                }
            }
            return Some(());
        };

        for (index, event) in events.into_iter().enumerate() {
            if !sink(event) {
                return None;
            }
            if index == snapshot_index {
                let snapshot = retained_snapshot.as_ref()?;
                visit_layer_snapshot_chunks(
                    snapshot.layer,
                    doc.size,
                    set_id,
                    snapshot.selected_after_id,
                    snapshot.override_image,
                    &mut sink,
                )?;
            }
        }
        Some(())
    }
}

fn push_bounded_event(events: &mut Vec<DocumentEvent>, event: DocumentEvent) -> bool {
    if events.len() >= MAX_RECOVERY_EVENT_COUNT || events.try_reserve(1).is_err() {
        return false;
    }
    events.push(event);
    true
}

fn make_layer_snapshot_begin(
    layer: &Layer,
    size: u32,
    set_id: u32,
    to_index: usize,
    selected_after_id: u32,
    override_image: Option<(LayerImageTarget, Option<&[u8]>)>,
) -> Option<(DocumentEvent, usize)> {
    if !(64..=crate::document::MAX_SIZE).contains(&size)
        || layer.id == 0
        || layer.name.is_empty()
        || layer.name.len() > 1024
        || !history_payload_within_limit(layer_storage_bytes(layer))
        || !layer
            .pixels
            .values()
            .all(|image| image.dimensions() == (size, size))
        || layer.mask.dimensions() != (size, size)
    {
        return None;
    }

    let mut channels = Vec::new();
    channels
        .try_reserve_exact(layer.pixels.len().saturating_add(1))
        .ok()?;
    for channel in Channel::ALL {
        let is_override = matches!(override_image, Some((LayerImageTarget::Channel(target), _)) if target == channel);
        let present = if is_override {
            override_image?.1.is_some()
        } else {
            layer.pixels.contains_key(&channel)
        };
        if present {
            channels.push(channel);
        }
    }
    let mut expected_channel_count = layer.pixels.len();
    if let Some((LayerImageTarget::Channel(channel), bytes)) = override_image {
        if bytes.is_some() && !layer.pixels.contains_key(&channel) {
            expected_channel_count = expected_channel_count.checked_add(1)?;
        } else if bytes.is_none() && layer.pixels.contains_key(&channel) {
            expected_channel_count = expected_channel_count.checked_sub(1)?;
        }
    }
    if channels.len() != expected_channel_count {
        return None;
    }
    if let Some((target, bytes)) = override_image {
        let expected_len = (size as usize).checked_mul(size as usize)?.checked_mul(
            if matches!(target, LayerImageTarget::Mask) {
                1
            } else {
                4
            },
        )?;
        if bytes.is_some_and(|bytes| bytes.len() != expected_len)
            || matches!(target, LayerImageTarget::Mask) && bytes.is_none()
        {
            return None;
        }
    }
    let mask_bytes = match override_image {
        Some((LayerImageTarget::Mask, Some(bytes))) => bytes,
        _ => layer.mask.as_raw(),
    };
    let chunk_count = std::iter::once(mask_bytes.len())
        .chain(channels.iter().map(|channel| match override_image {
            Some((LayerImageTarget::Channel(target), Some(bytes))) if target == *channel => {
                bytes.len()
            }
            _ => layer.pixels[channel].as_raw().len(),
        }))
        .try_fold(0usize, |sum, length| {
            sum.checked_add(length.div_ceil(RECOVERY_LAYER_CHUNK_BYTES))
        })?;
    let metadata = LayerSnapshotMetadata {
        name: layer.name.clone(),
        visible: layer.visible,
        opacity: layer.opacity,
        blend: layer.blend,
        disabled_channels: Channel::ALL
            .into_iter()
            .filter(|channel| layer.disabled_channels.contains(channel))
            .collect(),
        procedural_graph: layer.procedural_graph.clone(),
        extensions: layer.extensions.clone(),
    };
    let begin = DocumentEvent::LayerSnapshotBegin {
        set_id,
        layer_id: layer.id,
        to_index,
        selected_after_id,
        metadata,
        channels: channels.clone(),
    };
    if begin.encode_bounded().is_err() {
        return None;
    }
    Some((begin, chunk_count))
}

fn visit_layer_snapshot_chunks(
    layer: &Layer,
    size: u32,
    set_id: u32,
    selected_after_id: u32,
    override_image: Option<(LayerImageTarget, Option<&[u8]>)>,
    sink: &mut impl FnMut(DocumentEvent) -> bool,
) -> Option<()> {
    let mut visit_buffer = |target: LayerImageTarget, bytes: &[u8]| -> Option<()> {
        for (chunk_index, chunk) in bytes.chunks(RECOVERY_LAYER_CHUNK_BYTES).enumerate() {
            let offset = chunk_index
                .checked_mul(RECOVERY_LAYER_CHUNK_BYTES)
                .and_then(|offset| u64::try_from(offset).ok())?;
            let mut chunk_bytes = Vec::new();
            chunk_bytes.try_reserve_exact(chunk.len()).ok()?;
            chunk_bytes.extend_from_slice(chunk);
            let event = DocumentEvent::LayerSnapshotChunk {
                set_id,
                layer_id: layer.id,
                target,
                offset,
                bytes: chunk_bytes,
            };
            if !sink(event) {
                return None;
            }
        }
        Some(())
    };

    let mask_bytes = match override_image {
        Some((LayerImageTarget::Mask, Some(bytes))) => bytes,
        _ => layer.mask.as_raw(),
    };
    visit_buffer(LayerImageTarget::Mask, mask_bytes)?;
    for channel in Channel::ALL {
        let override_for_channel = match override_image {
            Some((LayerImageTarget::Channel(target), bytes)) if target == channel => Some(bytes),
            _ => None,
        };
        let image_bytes = if let Some(bytes) = override_for_channel {
            bytes
        } else if layer.pixels.contains_key(&channel) {
            Some(layer.pixels.get(&channel)?.as_raw().as_slice())
        } else {
            None
        };
        let Some(image_bytes) = image_bytes else {
            continue;
        };
        if image_bytes.len() != (size as usize).checked_mul(size as usize)?.checked_mul(4)? {
            return None;
        }
        visit_buffer(LayerImageTarget::Channel(channel), image_bytes)?;
    }
    let commit = DocumentEvent::LayerSnapshotCommit {
        set_id,
        layer_id: layer.id,
        selected_after_id,
    };
    if !sink(commit) {
        return None;
    }
    Some(())
}

fn restore_stroke_before_images(capture: &StrokeCapture, doc: &mut Document) {
    let size = doc.size;
    if !capture.mask && !capture.channel_was_present {
        if let Some(layer) = doc.paint_layer_mut(capture.layer_id) {
            layer.pixels.remove(&capture.channel);
        }
        return;
    }
    let bytes_per_pixel = if capture.mask { 1 } else { 4 };
    let stride = size as usize * bytes_per_pixel;
    let Some(target) =
        stroke_image_bytes_mut(doc, capture.layer_id, capture.mask, capture.channel, size)
    else {
        return;
    };
    for tile in capture.tiles.values() {
        let row_bytes = tile.width as usize * bytes_per_pixel;
        for (row_offset, row) in (tile.y..tile.y + tile.height).enumerate() {
            let target_start = row as usize * stride + tile.x as usize * bytes_per_pixel;
            let source_start = row_offset * row_bytes;
            target[target_start..target_start + row_bytes]
                .copy_from_slice(&tile.before[source_start..source_start + row_bytes]);
        }
    }
}

struct TileBefore {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    before: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrokeCommitResult {
    Committed,
    Unchanged,
    OverLimit,
}

struct Patch {
    offset: usize,
    row_bytes: usize,
    rows: usize,
    stride: usize,
    before: Vec<u8>,
    after: Vec<u8>,
}
struct Delta {
    layer_id: u32,
    channel: Channel,
    mask: bool,
    channel_was_present: bool,
    patches: Vec<Patch>,
    size: usize,
}

#[derive(Clone, PartialEq)]
pub struct LayerProperties {
    name: String,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    disabled_channels: HashSet<Channel>,
    procedural_graph: Option<ProceduralGraph>,
}

impl LayerProperties {
    pub fn capture(layer: &Layer) -> Self {
        Self {
            name: layer.name.clone(),
            visible: layer.visible,
            opacity: layer.opacity,
            blend: layer.blend,
            disabled_channels: layer.disabled_channels.clone(),
            procedural_graph: layer.procedural_graph.clone(),
        }
    }

    pub(crate) fn capture_node(node: &LayerNode) -> Self {
        match node {
            LayerNode::Paint(layer) => Self::capture(layer),
            LayerNode::Fill(fill) => Self::capture_common(&fill.properties, None),
            LayerNode::Group(group) => Self::capture_common(&group.properties, None),
        }
    }

    fn capture_common(properties: &LayerNodeProperties, graph: Option<ProceduralGraph>) -> Self {
        Self {
            name: properties.name.clone(),
            visible: properties.visible,
            opacity: properties.opacity,
            blend: properties.blend,
            disabled_channels: properties.disabled_channels.clone(),
            procedural_graph: graph,
        }
    }

    fn storage_bytes(&self) -> usize {
        self.name
            .len()
            .saturating_add(self.disabled_channels.len().saturating_mul(8))
            .saturating_add(std::mem::size_of::<bool>() * 2)
            .saturating_add(std::mem::size_of::<f32>())
            .saturating_add(std::mem::size_of::<BlendMode>())
            .saturating_add(self.procedural_graph.as_ref().map_or(0, |graph| {
                graph
                    .nodes
                    .capacity()
                    .saturating_mul(std::mem::size_of::<crate::procedural::ProceduralNode>())
                    .saturating_add(
                        graph
                            .edges
                            .capacity()
                            .saturating_mul(std::mem::size_of::<crate::procedural::GraphEdge>()),
                    )
            }))
    }

    fn apply(&self, layer: &mut Layer) {
        layer.name.clone_from(&self.name);
        layer.visible = self.visible;
        layer.opacity = self.opacity;
        layer.blend = self.blend;
        layer.disabled_channels.clone_from(&self.disabled_channels);
        layer.procedural_graph.clone_from(&self.procedural_graph);
    }

    fn apply_node(&self, node: &mut LayerNode) {
        match node {
            LayerNode::Paint(layer) => self.apply(layer),
            LayerNode::Fill(fill) => self.apply_common(&mut fill.properties),
            LayerNode::Group(group) => self.apply_common(&mut group.properties),
        }
    }

    fn apply_common(&self, properties: &mut LayerNodeProperties) {
        properties.name.clone_from(&self.name);
        properties.visible = self.visible;
        properties.opacity = self.opacity;
        properties.blend = self.blend;
        properties
            .disabled_channels
            .clone_from(&self.disabled_channels);
    }
}

#[allow(dead_code)]
enum StackChange {
    Added {
        name: String,
        duplicate_source: Option<u32>,
        /// Retained imported layer payload, present while the addition is undone.
        payload: Option<Layer>,
    },
    Removed {
        payload: Option<Layer>,
    },
    Moved {
        old_index: usize,
        new_index: usize,
    },
}

struct StackDelta {
    change: StackChange,
    layer_id: u32,
    selected_before: u32,
    selected_after: u32,
    index: usize,
}

enum TreeStructureChange {
    Inserted {
        payload: Option<LayerNode>,
    },
    Removed {
        payload: Option<LayerNode>,
    },
    Moved {
        from_parent_id: Option<u32>,
        from_index: usize,
        to_parent_id: Option<u32>,
        to_index: usize,
    },
}

struct TreeStructureDelta {
    change: TreeStructureChange,
    node_id: u32,
    selected_before_id: u32,
    selected_after_id: u32,
    parent_id: Option<u32>,
    index: usize,
}

struct FillValueDelta {
    node_id: u32,
    channel: Channel,
    before: Option<[u8; 4]>,
    after: Option<[u8; 4]>,
}

struct PropertyDelta {
    layer_id: u32,
    before: LayerProperties,
    after: LayerProperties,
}

#[derive(Clone, Copy)]
enum UniformTarget {
    Channel(Channel),
    Mask,
}

#[derive(Clone, Copy)]
enum UniformValue {
    Rgba([u8; 4]),
    Gray(u8),
}

/// A full-image uniform operation owns the inactive image buffer and swaps it
/// with the document on Undo/Redo. The compact value/hash metadata describes
/// the other side without cloning a second full image.
struct UniformDelta {
    layer_id: u32,
    target: UniformTarget,
    value: UniformValue,
    inactive: Option<Vec<u8>>,
    inactive_present: bool,
    inactive_is_after: bool,
    before_present: bool,
    before_hash: [u8; 32],
}

struct LayerSnapshotVisit<'a> {
    layer: &'a Layer,
    selected_after_id: u32,
    chunk_count: usize,
    override_image: Option<(LayerImageTarget, Option<&'a [u8]>)>,
}

impl UniformDelta {
    fn storage_bytes(&self) -> usize {
        self.inactive
            .as_ref()
            .map(Vec::capacity)
            .unwrap_or_default()
            .saturating_add(std::mem::size_of::<Self>())
    }
}

enum Entry {
    Pixels(Delta),
    Uniform(UniformDelta),
    MaskInvert {
        layer_id: u32,
    },
    Stack(StackDelta),
    TreeStructure(TreeStructureDelta),
    FillValue(FillValueDelta),
    Properties(PropertyDelta),
    /// Holds only the inactive mesh and bindings; Undo/Redo swaps ownership
    /// with the document to avoid retaining duplicate copies of both meshes.
    Mesh {
        mesh: Mesh,
        material_bindings: Vec<Option<u32>>,
        storage_bytes: usize,
    },
}

impl Entry {
    fn storage_bytes(&self) -> usize {
        match self {
            Self::Pixels(delta) => delta.size,
            Self::Uniform(delta) => delta.storage_bytes(),
            Self::MaskInvert { .. } => 0,
            Self::Stack(delta) => match &delta.change {
                StackChange::Added { name, payload, .. } => payload
                    .as_ref()
                    .map(layer_storage_bytes)
                    .unwrap_or_else(|| name.len()),
                StackChange::Removed { payload } => {
                    payload.as_ref().map(layer_storage_bytes).unwrap_or(0)
                }
                StackChange::Moved { .. } => 0,
            },
            Self::TreeStructure(delta) => match &delta.change {
                TreeStructureChange::Inserted { payload }
                | TreeStructureChange::Removed { payload } => {
                    payload.as_ref().map(node_storage_bytes).unwrap_or(0)
                }
                TreeStructureChange::Moved { .. } => 0,
            },
            Self::FillValue(_) => std::mem::size_of::<FillValueDelta>(),
            Self::Properties(delta) => delta
                .before
                .storage_bytes()
                .saturating_add(delta.after.storage_bytes()),
            Self::Mesh { storage_bytes, .. } => *storage_bytes,
        }
    }
}

fn mesh_storage_bytes(mesh: &Mesh, bindings_capacity: usize) -> usize {
    let vertex_bytes = mesh
        .vertices
        .capacity()
        .saturating_mul(std::mem::size_of::<crate::mesh::Vertex>())
        .saturating_add(mesh.vertices.iter().fold(0usize, |total, vertex| {
            total.saturating_add(json_map_storage_bytes(&vertex.extensions))
        }));
    let material_bytes = mesh
        .materials
        .capacity()
        .saturating_mul(std::mem::size_of::<String>())
        .saturating_add(
            mesh.materials
                .iter()
                .fold(0usize, |total, name| total.saturating_add(name.capacity())),
        );
    mesh.name
        .capacity()
        .saturating_add(vertex_bytes)
        .saturating_add(
            mesh.indices
                .capacity()
                .saturating_mul(std::mem::size_of::<u32>()),
        )
        .saturating_add(material_bytes)
        .saturating_add(
            mesh.triangle_materials
                .capacity()
                .saturating_mul(std::mem::size_of::<u32>()),
        )
        .saturating_add(
            mesh.extra_uv_sets
                .len()
                .saturating_mul(std::mem::size_of::<(u32, Vec<Option<[f32; 2]>>)>()),
        )
        .saturating_add(mesh.extra_uv_sets.values().fold(0usize, |total, values| {
            total.saturating_add(
                values
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Option<[f32; 2]>>()),
            )
        }))
        .saturating_add(json_map_storage_bytes(&mesh.extensions))
        .saturating_add(bindings_capacity.saturating_mul(std::mem::size_of::<Option<u32>>()))
}

fn layer_storage_bytes(layer: &Layer) -> usize {
    layer
        .mask
        .as_raw()
        .len()
        .saturating_add(
            layer
                .pixels
                .values()
                .map(|image| image.as_raw().len())
                .fold(0usize, usize::saturating_add),
        )
        .saturating_add(LayerProperties::capture(layer).storage_bytes())
        .saturating_add(json_map_storage_bytes(&layer.extensions))
}

fn clone_node_fallible(node: &LayerNode) -> std::io::Result<LayerNode> {
    fn clone_json_value(value: &serde_json::Value) -> std::io::Result<serde_json::Value> {
        use serde_json::Value;
        Ok(match value {
            Value::String(value) => {
                let mut copy = String::new();
                copy.try_reserve_exact(value.len())?;
                copy.push_str(value);
                Value::String(copy)
            }
            Value::Array(values) => {
                let mut copy = Vec::new();
                copy.try_reserve_exact(values.len())?;
                for value in values {
                    copy.push(clone_json_value(value)?);
                }
                Value::Array(copy)
            }
            Value::Object(values) => Value::Object(clone_json_map(values)?),
            value => value.clone(),
        })
    }

    fn clone_json_map(
        source: &serde_json::Map<String, serde_json::Value>,
    ) -> std::io::Result<serde_json::Map<String, serde_json::Value>> {
        let mut copy = serde_json::Map::new();
        for (key, value) in source {
            let mut key_copy = String::new();
            key_copy.try_reserve_exact(key.len())?;
            key_copy.push_str(key);
            copy.insert(key_copy, clone_json_value(value)?);
        }
        Ok(copy)
    }

    fn clone_properties(source: &LayerNodeProperties) -> std::io::Result<LayerNodeProperties> {
        let size = source.mask.width();
        let mut mask_bytes = Vec::new();
        mask_bytes.try_reserve_exact(source.mask.as_raw().len())?;
        mask_bytes.extend_from_slice(source.mask.as_raw());
        let mask = GrayImage::from_raw(size, source.mask.height(), mask_bytes)
            .ok_or_else(|| std::io::Error::other("invalid layer-tree mask dimensions"))?;
        let mut name = String::new();
        name.try_reserve_exact(source.name.len())?;
        name.push_str(&source.name);
        let mut disabled_channels = HashSet::new();
        disabled_channels.try_reserve(source.disabled_channels.len())?;
        disabled_channels.extend(source.disabled_channels.iter().copied());
        Ok(LayerNodeProperties {
            id: source.id,
            name,
            visible: source.visible,
            opacity: source.opacity,
            blend: source.blend,
            disabled_channels,
            mask,
            extensions: clone_json_map(&source.extensions)?,
        })
    }

    fn clone_layer(source: &Layer) -> std::io::Result<Layer> {
        let mut name = String::new();
        name.try_reserve_exact(source.name.len())?;
        name.push_str(&source.name);
        let mut mask_bytes = Vec::new();
        mask_bytes.try_reserve_exact(source.mask.as_raw().len())?;
        mask_bytes.extend_from_slice(source.mask.as_raw());
        let mask = GrayImage::from_raw(source.mask.width(), source.mask.height(), mask_bytes)
            .ok_or_else(|| std::io::Error::other("invalid paint-layer mask dimensions"))?;
        let mut pixels = HashMap::new();
        pixels.try_reserve(source.pixels.len())?;
        for (channel, image) in &source.pixels {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(image.as_raw().len())?;
            bytes.extend_from_slice(image.as_raw());
            let image = RgbaImage::from_raw(image.width(), image.height(), bytes)
                .ok_or_else(|| std::io::Error::other("invalid paint-layer channel dimensions"))?;
            pixels.insert(*channel, image);
        }
        let mut disabled_channels = HashSet::new();
        disabled_channels.try_reserve(source.disabled_channels.len())?;
        disabled_channels.extend(source.disabled_channels.iter().copied());
        Ok(Layer {
            id: source.id,
            name,
            pixels,
            visible: source.visible,
            opacity: source.opacity,
            blend: source.blend,
            disabled_channels,
            mask,
            procedural_graph: source.procedural_graph.clone(),
            extensions: clone_json_map(&source.extensions)?,
        })
    }

    fn clone_inner(
        node: &LayerNode,
        depth: usize,
        nodes: &mut usize,
    ) -> std::io::Result<LayerNode> {
        *nodes = nodes.saturating_add(1);
        if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH
            || *nodes > crate::layer_tree::MAX_LAYER_TREE_NODES
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "layer subtree exceeds structural limits",
            ));
        }
        Ok(match node {
            LayerNode::Paint(layer) => LayerNode::Paint(clone_layer(layer)?),
            LayerNode::Fill(fill) => {
                let mut values = HashMap::new();
                values.try_reserve(fill.values.len())?;
                values.extend(
                    fill.values
                        .iter()
                        .map(|(channel, value)| (*channel, *value)),
                );
                LayerNode::Fill(FillLayer {
                    properties: clone_properties(&fill.properties)?,
                    values,
                })
            }
            LayerNode::Group(group) => {
                let mut children = Vec::new();
                children.try_reserve_exact(group.children.len())?;
                for child in &group.children {
                    children.push(clone_inner(child, depth + 1, nodes)?);
                }
                LayerNode::Group(LayerGroup {
                    properties: clone_properties(&group.properties)?,
                    children,
                })
            }
        })
    }

    clone_inner(node, 1, &mut 0)
}

/// Conservatively account for every byte retained by a recursive subtree.
/// Image buffers are charged at their logical lengths, while collections and
/// owned strings/maps are charged at capacity so the 96-MiB limit includes
/// allocator-owned storage as well as pixels.
fn node_storage_bytes(node: &LayerNode) -> usize {
    let over_limit = MAX_HISTORY_BYTES.saturating_add(1);
    let mut total = 0usize;
    let mut extension_bytes = 0usize;
    let mut visited = 0usize;
    let mut pending = Vec::new();
    if pending.try_reserve(1).is_err() {
        return over_limit;
    }
    pending.push((node, 1usize));
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH
            || visited > crate::layer_tree::MAX_LAYER_TREE_NODES
        {
            return over_limit;
        }
        let node_bytes = match node {
            LayerNode::Paint(layer) => {
                extension_bytes =
                    extension_bytes.saturating_add(json_map_storage_bytes(&layer.extensions));
                layer_storage_bytes(layer)
                    .saturating_add(std::mem::size_of::<LayerNode>())
                    .saturating_add(layer.pixels.capacity().saturating_mul(std::mem::size_of::<(
                        Channel,
                        RgbaImage,
                    )>(
                    )))
                    .saturating_add(layer.name.capacity())
                    .saturating_add(
                        layer
                            .disabled_channels
                            .capacity()
                            .saturating_mul(std::mem::size_of::<Channel>()),
                    )
            }
            LayerNode::Fill(fill) => {
                extension_bytes = extension_bytes
                    .saturating_add(json_map_storage_bytes(&fill.properties.extensions));
                properties_storage_bytes(&fill.properties)
                    .saturating_add(
                        fill.values
                            .capacity()
                            .saturating_mul(std::mem::size_of::<(Channel, [u8; 4])>()),
                    )
                    .saturating_add(std::mem::size_of::<LayerNode>())
            }
            LayerNode::Group(group) => {
                extension_bytes = extension_bytes
                    .saturating_add(json_map_storage_bytes(&group.properties.extensions));
                let Some(next_len) = pending.len().checked_add(group.children.len()) else {
                    return over_limit;
                };
                if next_len > crate::layer_tree::MAX_LAYER_TREE_NODES
                    || pending.try_reserve(group.children.len()).is_err()
                {
                    return over_limit;
                }
                pending.extend(group.children.iter().map(|child| (child, depth + 1)));
                properties_storage_bytes(&group.properties)
                    .saturating_add(
                        group
                            .children
                            .capacity()
                            .saturating_mul(std::mem::size_of::<LayerNode>()),
                    )
                    .saturating_add(std::mem::size_of::<LayerNode>())
            }
        };
        if extension_bytes > MAX_TREE_EXTENSION_STORAGE_BYTES {
            return over_limit;
        }
        total = total.saturating_add(node_bytes);
        if total > MAX_HISTORY_BYTES {
            return over_limit;
        }
    }
    total
}

/// Bound all forward-compatible data in the live tree before retaining or
/// duplicating a subtree. The tree map is shared by every structural history
/// entry, so it is charged against the aggregate extension cap without being
/// counted as a duplicate retained payload.
fn tree_extension_storage_bytes(tree: &LayerTree) -> usize {
    let over_limit = MAX_TREE_EXTENSION_STORAGE_BYTES.saturating_add(1);
    let mut total = json_map_storage_bytes(&tree.extensions);
    if total > MAX_TREE_EXTENSION_STORAGE_BYTES {
        return over_limit;
    }
    let mut pending = Vec::new();
    if pending.try_reserve(tree.roots.len()).is_err() {
        return over_limit;
    }
    pending.extend(tree.roots.iter());
    let mut visited = 0usize;
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > crate::layer_tree::MAX_LAYER_TREE_NODES {
            return over_limit;
        }
        let (extensions, children): (&serde_json::Map<String, serde_json::Value>, &[LayerNode]) =
            match node {
                LayerNode::Paint(layer) => (&layer.extensions, &[]),
                LayerNode::Fill(fill) => (&fill.properties.extensions, &[]),
                LayerNode::Group(group) => (&group.properties.extensions, &group.children),
            };
        total = total.saturating_add(json_map_storage_bytes(extensions));
        if total > MAX_TREE_EXTENSION_STORAGE_BYTES
            || pending
                .len()
                .checked_add(children.len())
                .is_none_or(|count| count > crate::layer_tree::MAX_LAYER_TREE_NODES)
            || pending.try_reserve(children.len()).is_err()
        {
            return over_limit;
        }
        pending.extend(children);
    }
    total
}

fn subtree_extension_storage_bytes(root: &LayerNode) -> usize {
    let mut total = 0usize;
    let mut pending = Vec::new();
    if pending.try_reserve(1).is_err() {
        return MAX_TREE_EXTENSION_STORAGE_BYTES.saturating_add(1);
    }
    pending.push(root);
    let mut visited = 0usize;
    while let Some(node) = pending.pop() {
        visited += 1;
        if visited > crate::layer_tree::MAX_LAYER_TREE_NODES {
            return MAX_TREE_EXTENSION_STORAGE_BYTES.saturating_add(1);
        }
        let (extensions, children): (&serde_json::Map<String, serde_json::Value>, &[LayerNode]) =
            match node {
                LayerNode::Paint(layer) => (&layer.extensions, &[]),
                LayerNode::Fill(fill) => (&fill.properties.extensions, &[]),
                LayerNode::Group(group) => (&group.properties.extensions, &group.children),
            };
        total = total.saturating_add(json_map_storage_bytes(extensions));
        if total > MAX_TREE_EXTENSION_STORAGE_BYTES
            || pending
                .len()
                .checked_add(children.len())
                .is_none_or(|count| count > crate::layer_tree::MAX_LAYER_TREE_NODES)
            || pending.try_reserve(children.len()).is_err()
        {
            return MAX_TREE_EXTENSION_STORAGE_BYTES.saturating_add(1);
        }
        pending.extend(children);
    }
    total
}

fn decoded_bytes_for_node(root: &LayerNode) -> Option<u64> {
    let mut total = 0u64;
    let mut pending = vec![(root, 1usize)];
    let mut visited = 0usize;
    while let Some((node, depth)) = pending.pop() {
        visited = visited.checked_add(1)?;
        if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH
            || visited > crate::layer_tree::MAX_LAYER_TREE_NODES
        {
            return None;
        }
        let mask_bytes = match node {
            LayerNode::Paint(layer) => {
                total = total.checked_add(layer.mask.as_raw().len() as u64)?;
                for image in layer.pixels.values() {
                    total = total.checked_add(image.as_raw().len() as u64)?;
                }
                0
            }
            LayerNode::Fill(fill) => fill.properties.mask.as_raw().len() as u64,
            LayerNode::Group(group) => {
                pending.try_reserve(group.children.len()).ok()?;
                pending.extend(group.children.iter().map(|child| (child, depth + 1)));
                group.properties.mask.as_raw().len() as u64
            }
        };
        total = total.checked_add(mask_bytes)?;
    }
    Some(total)
}

fn subtree_contains_id(root: &LayerNode, id: u32) -> bool {
    let mut pending = vec![(root, 1usize)];
    let mut visited = 0usize;
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if node.id() == id {
            return true;
        }
        if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH
            || visited > crate::layer_tree::MAX_LAYER_TREE_NODES
        {
            return true;
        }
        if let LayerNode::Group(group) = node {
            if pending.len().saturating_add(group.children.len())
                > crate::layer_tree::MAX_LAYER_TREE_NODES
            {
                return true;
            }
            pending.extend(group.children.iter().map(|child| (child, depth + 1)));
        }
    }
    false
}

fn subtree_counts(root: &LayerNode) -> Option<(usize, usize)> {
    fn visit(node: &LayerNode, depth: usize, nodes: &mut usize, paints: &mut usize) -> Option<()> {
        if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH {
            return None;
        }
        *nodes = nodes.checked_add(1)?;
        if *nodes > crate::layer_tree::MAX_LAYER_TREE_NODES {
            return None;
        }
        match node {
            LayerNode::Paint(_) => *paints = paints.checked_add(1)?,
            LayerNode::Group(group) => {
                for child in &group.children {
                    visit(child, depth + 1, nodes, paints)?;
                }
            }
            LayerNode::Fill(_) => {}
        }
        Some(())
    }

    let mut nodes = 0;
    let mut paints = 0;
    visit(root, 1, &mut nodes, &mut paints)?;
    Some((nodes, paints))
}

fn assign_fresh_node_ids(
    node: &mut LayerNode,
    next_id: &mut u32,
    remap: &mut HashMap<u32, u32>,
) -> Option<()> {
    let old_id = node.id();
    let new_id = *next_id;
    *next_id = new_id.checked_add(1)?;
    remap.insert(old_id, new_id);
    match node {
        LayerNode::Paint(layer) => layer.id = new_id,
        LayerNode::Fill(fill) => fill.properties.id = new_id,
        LayerNode::Group(group) => {
            group.properties.id = new_id;
            for child in &mut group.children {
                assign_fresh_node_ids(child, next_id, remap)?;
            }
        }
    }
    Some(())
}

fn remap_internal_procedural_sources(node: &mut LayerNode, remap: &HashMap<u32, u32>) {
    match node {
        LayerNode::Paint(layer) => {
            if let Some(graph) = &mut layer.procedural_graph {
                for graph_node in &mut graph.nodes {
                    if let crate::procedural::NodeKind::OcclusionInput { source_layer_id } =
                        &mut graph_node.kind
                    {
                        if let Some(&mapped) = remap.get(source_layer_id) {
                            *source_layer_id = mapped;
                        }
                    }
                }
            }
        }
        LayerNode::Fill(_) => {}
        LayerNode::Group(group) => {
            for child in &mut group.children {
                remap_internal_procedural_sources(child, remap);
            }
        }
    }
}

fn duplicate_node_name(name: &str) -> String {
    const SUFFIX: &str = " copy";
    let max_prefix_len = 1024 - SUFFIX.len();
    let mut prefix_len = name.len().min(max_prefix_len);
    while !name.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    let mut duplicate = String::with_capacity(prefix_len + SUFFIX.len());
    duplicate.push_str(&name[..prefix_len]);
    duplicate.push_str(SUFFIX);
    duplicate
}

fn properties_storage_bytes(properties: &LayerNodeProperties) -> usize {
    properties
        .mask
        .as_raw()
        .len()
        .saturating_add(properties.name.capacity())
        .saturating_add(
            properties
                .disabled_channels
                .capacity()
                .saturating_mul(std::mem::size_of::<Channel>()),
        )
        .saturating_add(json_map_storage_bytes(&properties.extensions))
}

// Estimate owned JSON heap storage conservatively. The serialized length alone
// misses container nodes and can be much smaller than the memory retained by
// a removed layer's extension fields.
fn json_map_storage_bytes(map: &serde_json::Map<String, serde_json::Value>) -> usize {
    map.iter().fold(0usize, |total, (key, value)| {
        total
            .saturating_add(key.capacity())
            .saturating_add(std::mem::size_of::<usize>() * 4)
            .saturating_add(json_value_storage_bytes(value))
    })
}

fn json_value_storage_bytes(value: &serde_json::Value) -> usize {
    let inline = std::mem::size_of::<serde_json::Value>();
    inline.saturating_add(match value {
        serde_json::Value::String(value) => value.capacity(),
        serde_json::Value::Array(values) => values
            .iter()
            .fold(values.capacity().saturating_mul(inline), |total, value| {
                total.saturating_add(json_value_storage_bytes(value))
            }),
        serde_json::Value::Object(values) => json_map_storage_bytes(values),
        _ => 0,
    })
}

fn layer_image_bytes(layer: &Layer) -> u64 {
    layer.mask.as_raw().len().saturating_add(
        layer
            .pixels
            .values()
            .map(|image| image.as_raw().len())
            .fold(0usize, usize::saturating_add),
    ) as u64
}

fn set_selection(doc: &mut Document, layer_id: u32) -> bool {
    doc.layer_tree.select(layer_id).is_ok()
}

fn mesh_state_is_valid(doc: &Document, mesh: &Mesh, bindings: &[Option<u32>]) -> bool {
    mesh.validate().is_ok()
        && bindings.len() == mesh.material_count()
        && bindings.iter().all(|binding| match binding {
            None => true,
            Some(id) => {
                *id != 0
                    && (*id == doc.active_set_id
                        || doc.texture_sets.iter().any(|set| set.id == *id))
            }
        })
}

pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    used_bytes: usize,
}

fn history_payload_within_limit(bytes: usize) -> bool {
    bytes <= MAX_HISTORY_BYTES
}

fn snapshot_storage_bytes(before: &[u8], after: &[u8]) -> usize {
    before
        .chunks(CHUNK_BYTES)
        .zip(after.chunks(CHUNK_BYTES))
        .filter(|(before, after)| before != after)
        .fold(0usize, |stored, (before, _)| {
            stored
                .saturating_add(before.len().saturating_mul(2))
                .saturating_add(std::mem::size_of::<Patch>())
        })
}

fn snapshot_upper_bound_fits(size: u32, bytes_per_pixel: usize) -> bool {
    if size == 0 || bytes_per_pixel == 0 {
        return false;
    }
    let Some(image_bytes) = (size as usize)
        .checked_mul(size as usize)
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
    else {
        return false;
    };
    let chunks = image_bytes.div_ceil(CHUNK_BYTES);
    image_bytes
        .checked_mul(2)
        .and_then(|bytes| {
            chunks
                .checked_mul(std::mem::size_of::<Patch>())
                .and_then(|overhead| bytes.checked_add(overhead))
        })
        .is_some_and(|bytes| bytes <= MAX_HISTORY_BYTES)
}

fn uniform_state_hash(bytes: Option<&[u8]>) -> [u8; 32] {
    Sha256::digest(bytes.unwrap_or_default()).into()
}

fn uniform_value_matches(bytes: &[u8], value: UniformValue) -> bool {
    match value {
        UniformValue::Rgba(value) => bytes.chunks_exact(4).all(|pixel| pixel == value),
        UniformValue::Gray(value) => bytes.iter().all(|pixel| *pixel == value),
    }
}

impl History {
    /// Whether a full-image snapshot can fit the history limit even if every
    /// byte changes. Callers that cannot cancel or roll back an edit must
    /// check this before they allocate snapshots or mutate the document.
    pub(crate) fn snapshot_upper_bound_fits(size: u32, bytes_per_pixel: usize) -> bool {
        snapshot_upper_bound_fits(size, bytes_per_pixel)
    }

    /// Reserve one history slot before a full-image edit allocates or mutates.
    /// The caller supplies the maximum inactive image capacity it will retain.
    pub(crate) fn prepare_uniform_edit(&mut self, storage_bytes: usize) -> bool {
        storage_bytes.saturating_add(std::mem::size_of::<UniformDelta>()) <= MAX_HISTORY_BYTES
            && self.undo.try_reserve(1).is_ok()
    }

    pub(crate) fn record_uniform_channel(
        &mut self,
        layer_id: u32,
        channel: Channel,
        value: [u8; 4],
        previous: Option<RgbaImage>,
    ) {
        let inactive = previous.map(RgbaImage::into_raw);
        let before_hash = uniform_state_hash(inactive.as_deref());
        self.push(Entry::Uniform(UniformDelta {
            layer_id,
            target: UniformTarget::Channel(channel),
            value: UniformValue::Rgba(value),
            inactive_present: inactive.is_some(),
            before_present: inactive.is_some(),
            inactive,
            inactive_is_after: false,
            before_hash,
        }));
    }

    pub(crate) fn record_uniform_mask(&mut self, layer_id: u32, value: u8, previous: GrayImage) {
        let inactive = Some(previous.into_raw());
        let before_hash = uniform_state_hash(inactive.as_deref());
        self.push(Entry::Uniform(UniformDelta {
            layer_id,
            target: UniformTarget::Mask,
            value: UniformValue::Gray(value),
            inactive_present: true,
            before_present: true,
            inactive,
            inactive_is_after: false,
            before_hash,
        }));
    }

    pub(crate) fn record_mask_invert(&mut self, layer_id: u32) {
        self.push(Entry::MaskInvert { layer_id });
    }

    /// Retain the previous mesh state as one undo entry. If validation or the
    /// history budget rejects it, leave both existing stacks untouched so an
    /// unrecordable import does not discard older, still-useful Undo entries.
    pub(crate) fn record_mesh_replacement(
        &mut self,
        previous_mesh: Mesh,
        previous_bindings: Vec<Option<u32>>,
    ) -> bool {
        if previous_mesh.validate().is_err()
            || previous_bindings.len() != previous_mesh.material_count()
            || previous_bindings.contains(&Some(0))
        {
            return false;
        }
        let storage_bytes = mesh_storage_bytes(&previous_mesh, previous_bindings.capacity());
        if storage_bytes > MAX_HISTORY_BYTES {
            return false;
        }
        self.push(Entry::Mesh {
            mesh: previous_mesh,
            material_bindings: previous_bindings,
            storage_bytes,
        });
        true
    }

    /// Return the retained state that the next mesh Undo or Redo will install.
    pub(crate) fn mesh_recovery_target(
        &self,
        direction: HistoryDirection,
    ) -> Option<(&Mesh, &[Option<u32>])> {
        let entry = match direction {
            HistoryDirection::Undo => self.undo.last()?,
            HistoryDirection::Redo => self.redo.last()?,
        };
        match entry {
            Entry::Mesh {
                mesh,
                material_bindings,
                ..
            } => Some((mesh, material_bindings)),
            _ => None,
        }
    }

    pub fn new() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            used_bytes: 0,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.used_bytes = 0;
    }
    /// Capture an editable channel without creating it. An absent channel is
    /// represented by its logical all-zero image, while masks are always
    /// present and are copied directly.
    pub fn capture(doc: &Document, channel: Channel, mode: BrushMode) -> Vec<u8> {
        let Some(layer) = doc.selected_layer() else {
            return Vec::new();
        };
        if mode.is_mask() {
            layer.mask.as_raw().clone()
        } else {
            layer
                .pixels
                .get(&channel)
                .map(|image| image.as_raw().clone())
                .unwrap_or_else(|| vec![0; doc.size as usize * doc.size as usize * 4])
        }
    }

    pub fn begin_stroke(doc: &Document, channel: Channel, mode: BrushMode) -> StrokeCapture {
        let selected_id = doc.layer_tree.selected_node_id;
        let selected_node = doc.layer_tree.find_node(selected_id);
        let channel_was_present = if mode.is_mask() {
            selected_node.is_some()
        } else {
            selected_node.is_some_and(|node| match node {
                LayerNode::Paint(layer) => layer.pixels.contains_key(&channel),
                LayerNode::Fill(_) | LayerNode::Group(_) => false,
            })
        };
        if selected_node.is_none()
            || (!mode.is_mask()
                && !channel_was_present
                && !matches!(selected_node, Some(LayerNode::Paint(_))))
        {
            return StrokeCapture {
                layer_id: selected_id,
                channel,
                mask: mode.is_mask(),
                channel_was_present: false,
                size: doc.size,
                tiles: HashMap::new(),
                reserved_bytes: 0,
                over_limit: true,
            };
        }
        StrokeCapture {
            layer_id: selected_id,
            channel,
            mask: mode.is_mask(),
            channel_was_present: mode.is_mask() || channel_was_present,
            size: doc.size,
            tiles: HashMap::new(),
            reserved_bytes: 0,
            over_limit: false,
        }
    }

    /// Capture original pixels for a stamp's inclusive bounding rectangle.
    /// Repeated stamps in one tile retain only its first before-image.
    pub fn capture_stroke_rect(
        capture: &mut StrokeCapture,
        doc: &Document,
        min_x: u32,
        min_y: u32,
        max_x: u32,
        max_y: u32,
    ) -> bool {
        if capture.over_limit {
            return false;
        }
        if min_x > max_x || min_y > max_y || capture.size != doc.size {
            return true;
        }
        let size = capture.size;
        let tile_columns = size.div_ceil(STROKE_TILE_SIZE) as usize;
        let first_tile_x = min_x / STROKE_TILE_SIZE;
        let first_tile_y = min_y / STROKE_TILE_SIZE;
        let last_tile_x = max_x.min(size - 1) / STROKE_TILE_SIZE;
        let last_tile_y = max_y.min(size - 1) / STROKE_TILE_SIZE;
        let Some(node) = doc.layer_tree.find_node(capture.layer_id) else {
            capture.over_limit = true;
            capture.tiles.clear();
            capture.reserved_bytes = 0;
            return false;
        };
        let source = stroke_image_bytes(node, capture.mask, capture.channel);
        let bytes_per_pixel = if capture.mask { 1 } else { 4 };
        let stride = size as usize * bytes_per_pixel;

        for tile_y in first_tile_y..=last_tile_y {
            for tile_x in first_tile_x..=last_tile_x {
                let key = tile_y as usize * tile_columns + tile_x as usize;
                if capture.tiles.contains_key(&key) {
                    continue;
                }
                let x = tile_x * STROKE_TILE_SIZE;
                let y = tile_y * STROKE_TILE_SIZE;
                let width = STROKE_TILE_SIZE.min(size - x);
                let height = STROKE_TILE_SIZE.min(size - y);
                let row_bytes = width as usize * bytes_per_pixel;
                let payload_bytes = row_bytes.saturating_mul(height as usize);
                let projected = payload_bytes
                    .saturating_mul(2)
                    .saturating_add(std::mem::size_of::<Patch>())
                    .saturating_add(std::mem::size_of::<(usize, TileBefore)>() * 2);
                if capture.reserved_bytes.saturating_add(projected) > MAX_HISTORY_BYTES
                    || capture.tiles.try_reserve(1).is_err()
                {
                    capture.over_limit = true;
                    return false;
                }
                let mut before = Vec::new();
                if before.try_reserve_exact(payload_bytes).is_err() {
                    capture.over_limit = true;
                    return false;
                }
                if let Some(source) = source {
                    for row in y..y + height {
                        let start = row as usize * stride + x as usize * bytes_per_pixel;
                        before.extend_from_slice(&source[start..start + row_bytes]);
                    }
                } else {
                    before.resize(payload_bytes, 0);
                }
                capture.tiles.insert(
                    key,
                    TileBefore {
                        x,
                        y,
                        width,
                        height,
                        before,
                    },
                );
                capture.reserved_bytes = capture.reserved_bytes.saturating_add(projected);
            }
        }
        true
    }

    /// Finalize a brush stroke from its touched tiles. Oversized strokes are
    /// rolled back before returning, leaving existing undo and redo intact.
    pub fn commit_stroke(
        &mut self,
        capture: StrokeCapture,
        doc: &mut Document,
    ) -> StrokeCommitResult {
        if capture.over_limit {
            restore_stroke_before_images(&capture, doc);
            return StrokeCommitResult::OverLimit;
        }
        let bytes_per_pixel = if capture.mask { 1 } else { 4 };
        let stride = capture.size as usize * bytes_per_pixel;
        let after_tiles = match collect_stroke_after_images(&capture, doc, bytes_per_pixel, stride)
        {
            Some(after_tiles) => after_tiles,
            None => {
                restore_stroke_before_images(&capture, doc);
                return StrokeCommitResult::OverLimit;
            }
        };
        if after_tiles.is_empty() {
            return StrokeCommitResult::Unchanged;
        }
        let mut stored = 0usize;
        for (key, after) in &after_tiles {
            let before = &capture.tiles[key].before;
            stored = stored
                .saturating_add(before.len().saturating_add(after.len()))
                .saturating_add(std::mem::size_of::<Patch>());
        }
        if stored > MAX_HISTORY_BYTES {
            restore_stroke_before_images(&capture, doc);
            return StrokeCommitResult::OverLimit;
        }
        let mut patches = Vec::new();
        if patches.try_reserve_exact(after_tiles.len()).is_err() {
            restore_stroke_before_images(&capture, doc);
            return StrokeCommitResult::OverLimit;
        }
        let mut capture = capture;
        for (key, after) in after_tiles {
            let tile = capture.tiles.remove(&key).expect("tile was captured");
            patches.push(Patch {
                offset: tile.y as usize * stride + tile.x as usize * bytes_per_pixel,
                row_bytes: tile.width as usize * bytes_per_pixel,
                rows: tile.height as usize,
                stride,
                before: tile.before,
                after,
            });
        }
        self.push(Entry::Pixels(Delta {
            layer_id: capture.layer_id,
            channel: capture.channel,
            mask: capture.mask,
            channel_was_present: capture.channel_was_present,
            patches,
            size: stored,
        }));
        StrokeCommitResult::Committed
    }
    pub fn commit(&mut self, entry: Snapshot) -> bool {
        if entry.before.len() != entry.after.len() || entry.before == entry.after {
            return false;
        }
        let stored = snapshot_storage_bytes(&entry.before, &entry.after);
        self.commit_snapshot_with_storage(entry, stored)
    }

    fn commit_snapshot_with_storage(&mut self, entry: Snapshot, stored: usize) -> bool {
        if entry.before.len() != entry.after.len() || entry.before == entry.after {
            return false;
        }
        if !self.prepare_snapshot_commit(stored) {
            return false;
        }

        let mut patches = Vec::new();
        for (index, (before, after)) in entry
            .before
            .chunks(CHUNK_BYTES)
            .zip(entry.after.chunks(CHUNK_BYTES))
            .enumerate()
        {
            if before == after {
                continue;
            }
            patches.push(Patch {
                offset: index * CHUNK_BYTES,
                row_bytes: before.len(),
                rows: 1,
                stride: before.len(),
                before: before.to_vec(),
                after: after.to_vec(),
            });
        }
        self.push(Entry::Pixels(Delta {
            layer_id: entry.layer_id,
            channel: entry.channel,
            mask: entry.mask,
            channel_was_present: entry.channel_was_present,
            patches,
            size: stored,
        }));
        true
    }

    fn prepare_snapshot_commit(&mut self, stored: usize) -> bool {
        // The edit is already applied, so an unretained edit invalidates redo.
        // Keep older undo entries and account only for the retained undo stack.
        if stored > MAX_HISTORY_BYTES {
            self.redo.clear();
            self.recompute_bytes();
            return false;
        }
        true
    }

    #[cfg(test)]
    pub fn can_retain_removed_layer(layer: &Layer) -> bool {
        history_payload_within_limit(layer_storage_bytes(layer))
    }

    #[cfg(test)]
    pub fn can_retain_subtree(node: &LayerNode) -> bool {
        history_payload_within_limit(node_storage_bytes(node))
    }

    /// Preflight a new subtree against both its retained-history footprint and
    /// the live tree's aggregate forward-compatible extension limit.
    pub fn can_retain_subtree_in_tree(tree: &LayerTree, node: &LayerNode) -> bool {
        history_payload_within_limit(node_storage_bytes(node))
            && tree_extension_storage_bytes(tree)
                .saturating_add(subtree_extension_storage_bytes(node))
                <= MAX_TREE_EXTENSION_STORAGE_BYTES
    }

    /// Deep-copy a selected node and insert it beside the source. Every copied
    /// node receives a fresh stable ID; procedural references into the copied
    /// subtree follow those IDs, while references outside it remain unchanged.
    pub fn duplicate_subtree(
        &mut self,
        doc: &mut Document,
        source_id: u32,
    ) -> Result<u32, &'static str> {
        let tree = &doc.layer_tree;
        let source = tree
            .find_node(source_id)
            .ok_or("Selected layer no longer exists")?;
        let (subtree_nodes, subtree_paints) =
            subtree_counts(source).ok_or("Layer subtree exceeds structural limits")?;
        let node_ids = tree.all_node_ids();
        if node_ids
            .len()
            .checked_add(subtree_nodes)
            .is_none_or(|count| count > crate::layer_tree::MAX_LAYER_TREE_NODES)
        {
            return Err("Duplicated subtree exceeds the layer-tree node limit");
        }
        if tree
            .ordered_paint_leaf_ids()
            .len()
            .checked_add(subtree_paints)
            .is_none_or(|count| count > MAX_LAYERS)
        {
            return Err("Duplicated subtree exceeds the Paint layer limit");
        }
        if !Self::can_retain_subtree_in_tree(tree, source) {
            return Err("Layer subtree exceeds the 96-MiB undo or 48-MiB extension limit");
        }
        let decoded = decoded_bytes_for_node(source)
            .ok_or("Layer subtree exceeds structural or byte limits")?;
        if doc
            .decoded_bytes()
            .checked_add(decoded)
            .is_none_or(|used| used > crate::document::MAX_DECODED_BYTES)
        {
            return Err("Layer subtree exceeds the 512-MiB decoded texture budget");
        }
        let count_u32 = u32::try_from(subtree_nodes)
            .map_err(|_| "Layer subtree exceeds the stable-ID limit")?;
        tree.next_node_id
            .checked_add(count_u32)
            .ok_or("Layer tree node ID counter exhausted")?;
        let parent_id = tree
            .parent_id(source_id)
            .ok_or("Selected layer no longer exists")?;
        let source_index = tree
            .path_to(source_id)
            .and_then(|path| path.last().copied())
            .ok_or("Selected layer no longer exists")?;
        let source_name = source.name().to_owned();

        let mut duplicate =
            clone_node_fallible(source).map_err(|_| "Could not reserve duplicate subtree")?;
        let mut remap = HashMap::new();
        remap
            .try_reserve(subtree_nodes)
            .map_err(|_| "Could not reserve duplicate layer IDs")?;
        let mut next_id = tree.next_node_id;
        assign_fresh_node_ids(&mut duplicate, &mut next_id, &mut remap)
            .ok_or("Layer tree node ID counter exhausted")?;
        remap_internal_procedural_sources(&mut duplicate, &remap);
        let duplicate_name = duplicate_node_name(&source_name);
        match &mut duplicate {
            LayerNode::Paint(layer) => layer.name = duplicate_name,
            LayerNode::Fill(fill) => fill.properties.name = duplicate_name,
            LayerNode::Group(group) => group.properties.name = duplicate_name,
        }
        let duplicate_id = duplicate.id();
        self.insert_subtree(doc, parent_id, source_index + 1, duplicate, duplicate_id)?;
        Ok(duplicate_id)
    }

    /// Insert a subtree and register its undo payload atomically with respect
    /// to history admission. All size/count checks and retained-copy allocation
    /// happen before the live tree changes.
    pub fn insert_subtree(
        &mut self,
        doc: &mut Document,
        parent_id: Option<u32>,
        index: usize,
        node: LayerNode,
        selected_after_id: u32,
    ) -> Result<(), &'static str> {
        let bytes = node_storage_bytes(&node);
        let decoded = decoded_bytes_for_node(&node)
            .ok_or("Layer subtree exceeds structural or byte limits")?;
        if !history_payload_within_limit(bytes) {
            return Err("Layer subtree exceeds the 96-MiB undo payload limit");
        }
        if !Self::can_retain_subtree_in_tree(&doc.layer_tree, &node) {
            return Err("Layer extensions exceed the 48-MiB aggregate limit");
        }
        if doc
            .decoded_bytes()
            .checked_add(decoded)
            .is_none_or(|used| used > crate::document::MAX_DECODED_BYTES)
        {
            return Err("Layer subtree exceeds the 512-MiB decoded texture budget");
        }
        if doc.layer_tree.find_node(selected_after_id).is_none()
            && !subtree_contains_id(&node, selected_after_id)
        {
            return Err("Selected node after insertion does not exist");
        }
        let selected_before_id = doc.layer_tree.selected_node_id;
        let node_id = node.id();
        let retained =
            clone_node_fallible(&node).map_err(|_| "Could not reserve layer history payload")?;
        self.undo
            .try_reserve(1)
            .map_err(|_| "Could not reserve layer history entry")?;
        doc.layer_tree
            .insert(parent_id, index, node)
            .map_err(|_| "Could not insert layer subtree")?;
        doc.layer_tree
            .select(selected_after_id)
            .map_err(|_| "Selected node after insertion is invalid")?;
        self.push(Entry::TreeStructure(TreeStructureDelta {
            change: TreeStructureChange::Inserted {
                payload: Some(retained),
            },
            node_id,
            selected_before_id,
            selected_after_id,
            parent_id,
            index,
        }));
        Ok(())
    }

    /// Remove a node and retain its entire subtree as one undo entry. Every
    /// precondition and the 96-MiB payload limit is checked before mutation.
    pub fn remove_subtree(
        &mut self,
        doc: &mut Document,
        node_id: u32,
        selected_after_id: u32,
    ) -> Result<bool, &'static str> {
        let node = doc
            .layer_tree
            .find_node(node_id)
            .ok_or("Unknown layer-tree node")?;
        if tree_extension_storage_bytes(&doc.layer_tree) > MAX_TREE_EXTENSION_STORAGE_BYTES {
            return Err("Layer extensions exceed the 48-MiB aggregate limit");
        }
        let bytes = node_storage_bytes(node);
        if !history_payload_within_limit(bytes) {
            return Err("Layer subtree exceeds the 96-MiB undo payload limit");
        }
        if subtree_contains_id(node, selected_after_id)
            || doc.layer_tree.find_node(selected_after_id).is_none()
        {
            return Err("Selected node must survive outside the removed subtree");
        }
        let parent_id = doc
            .layer_tree
            .parent_id(node_id)
            .ok_or("Unknown layer-tree node")?;
        let path = doc
            .layer_tree
            .path_to(node_id)
            .ok_or("Unknown layer-tree node")?;
        let index = *path.last().ok_or("Layer-tree node has no sibling index")?;
        if doc
            .layer_tree
            .siblings(parent_id)
            .map_err(|_| "Unknown layer-tree parent")?
            .len()
            == 1
            && parent_id.is_none()
        {
            return Ok(false);
        }
        let selected_before_id = doc.layer_tree.selected_node_id;
        self.undo
            .try_reserve(1)
            .map_err(|_| "Could not reserve layer history entry")?;
        let removed = doc
            .layer_tree
            .remove(node_id, selected_after_id)
            .map_err(|_| "Could not remove layer subtree")?;
        self.push(Entry::TreeStructure(TreeStructureDelta {
            change: TreeStructureChange::Removed {
                payload: Some(removed),
            },
            node_id,
            selected_before_id,
            selected_after_id,
            parent_id,
            index,
        }));
        Ok(true)
    }

    /// Move a subtree between sibling lists and retain exact source/destination
    /// coordinates for Undo/Redo. `LayerTree::move_node` validates cycles and
    /// depth before it mutates the tree.
    pub fn move_subtree(
        &mut self,
        doc: &mut Document,
        node_id: u32,
        to_parent_id: Option<u32>,
        to_index: usize,
    ) -> Result<bool, &'static str> {
        let from_parent_id = doc
            .layer_tree
            .parent_id(node_id)
            .ok_or("Unknown layer-tree node")?;
        if tree_extension_storage_bytes(&doc.layer_tree) > MAX_TREE_EXTENSION_STORAGE_BYTES {
            return Err("Layer extensions exceed the 48-MiB aggregate limit");
        }
        let path = doc
            .layer_tree
            .path_to(node_id)
            .ok_or("Unknown layer-tree node")?;
        let from_index = *path.last().ok_or("Layer-tree node has no sibling index")?;
        if from_parent_id == to_parent_id && from_index == to_index {
            return Ok(false);
        }
        let selected_node_id = doc.layer_tree.selected_node_id;
        self.undo
            .try_reserve(1)
            .map_err(|_| "Could not reserve layer history entry")?;
        doc.layer_tree
            .move_node(node_id, to_parent_id, to_index)
            .map_err(|_| "Could not move layer subtree")?;
        self.push(Entry::TreeStructure(TreeStructureDelta {
            change: TreeStructureChange::Moved {
                from_parent_id,
                from_index,
                to_parent_id,
                to_index,
            },
            node_id,
            selected_before_id: selected_node_id,
            selected_after_id: selected_node_id,
            parent_id: from_parent_id,
            index: from_index,
        }));
        Ok(true)
    }

    pub fn record_fill_value(
        &mut self,
        node_id: u32,
        channel: Channel,
        before: Option<[u8; 4]>,
        after: Option<[u8; 4]>,
    ) {
        if before == after {
            return;
        }
        self.push(Entry::FillValue(FillValueDelta {
            node_id,
            channel,
            before,
            after,
        }));
    }

    #[cfg(test)]
    pub fn record_added_layer(
        &mut self,
        doc: &Document,
        layer_id: u32,
        selected_before: u32,
        duplicate_source: Option<u32>,
    ) {
        if doc.layer_tree.parent_id(layer_id) != Some(None) {
            return;
        }
        let Some(index) = doc
            .layer_tree
            .roots
            .iter()
            .position(|node| node.id() == layer_id)
        else {
            return;
        };
        let Some(LayerNode::Paint(layer)) = doc.layer_tree.find_node(layer_id) else {
            return;
        };
        let selected_after = doc.layer_tree.selected_node_id;
        let name = layer.name.clone();
        self.push(Entry::Stack(StackDelta {
            change: StackChange::Added {
                name,
                duplicate_source,
                payload: None,
            },
            layer_id,
            selected_before,
            selected_after,
            index,
        }));
    }

    /// Record an added layer whose contents cannot be reconstructed from a
    /// source layer (for example, an imported texture). The retained payload is
    /// moved back into the document on redo and counts toward the history cap.
    pub fn record_added_layer_with_payload(
        &mut self,
        doc: &Document,
        layer_id: u32,
        selected_before: u32,
    ) -> bool {
        if doc.layer_tree.parent_id(layer_id) != Some(None) {
            return false;
        }
        let Some(index) = doc
            .layer_tree
            .roots
            .iter()
            .position(|node| node.id() == layer_id)
        else {
            return false;
        };
        let Some(layer) = doc.paint_layer(layer_id) else {
            return false;
        };
        if !history_payload_within_limit(layer_storage_bytes(layer)) {
            return false;
        }
        let selected_after = doc.layer_tree.selected_node_id;
        self.push(Entry::Stack(StackDelta {
            change: StackChange::Added {
                name: layer.name.clone(),
                duplicate_source: None,
                payload: Some(layer.clone()),
            },
            layer_id,
            selected_before,
            selected_after,
            index,
        }));
        true
    }

    #[cfg(test)]
    pub fn record_removed_layer(
        &mut self,
        layer: Layer,
        index: usize,
        selected_before: u32,
        selected_after: u32,
    ) {
        let layer_id = layer.id;
        self.push(Entry::Stack(StackDelta {
            change: StackChange::Removed {
                payload: Some(layer),
            },
            layer_id,
            selected_before,
            selected_after,
            index,
        }));
    }

    #[cfg(test)]
    pub fn remove_selected_layer(
        &mut self,
        doc: &mut Document,
    ) -> std::result::Result<bool, &'static str> {
        let selected_id = doc.layer_tree.selected_node_id;
        let Some(LayerNode::Paint(layer)) = doc.layer_tree.find_node(selected_id) else {
            return Ok(false);
        };
        if !Self::can_retain_removed_layer(layer) {
            return Err("Layer exceeds the 96-MiB undo payload limit");
        }
        let selected_before = selected_id;
        let parent_id = doc.layer_tree.parent_id(selected_id).flatten();
        let path = doc
            .layer_tree
            .path_to(selected_id)
            .ok_or("Selected layer is missing")?;
        let index = *path.last().ok_or("Selected layer path is empty")?;
        let siblings = doc
            .layer_tree
            .siblings(parent_id)
            .map_err(|_| "Selected layer parent is missing")?;
        if siblings.len() <= 1 && parent_id.is_none() {
            return Ok(false);
        }
        let selected_after = siblings
            .iter()
            .find(|node| node.id() != selected_id)
            .map(LayerNode::id)
            .ok_or("No surviving layer node")?;
        let removed = doc
            .layer_tree
            .remove(selected_id, selected_after)
            .map_err(|_| "Could not remove selected layer")?;
        let LayerNode::Paint(removed) = removed else {
            return Err("Selected node is not a Paint layer");
        };
        self.record_removed_layer(removed, index, selected_before, selected_after);
        Ok(true)
    }

    #[cfg(test)]
    pub fn record_moved_layer(
        &mut self,
        layer_id: u32,
        old_index: usize,
        new_index: usize,
        selected_before: u32,
        selected_after: u32,
    ) {
        self.push(Entry::Stack(StackDelta {
            change: StackChange::Moved {
                old_index,
                new_index,
            },
            layer_id,
            selected_before,
            selected_after,
            index: new_index,
        }));
    }

    pub fn record_properties(
        &mut self,
        layer_id: u32,
        before: LayerProperties,
        after: LayerProperties,
    ) {
        if before == after {
            return;
        }
        self.redo.clear();
        if let Some(Entry::Properties(previous)) = self.undo.last_mut() {
            if previous.layer_id == layer_id && previous.after == before {
                previous.after = after;
                if previous.before == previous.after {
                    self.undo.pop();
                }
                self.recompute_bytes();
                self.evict_oldest();
                return;
            }
        }
        self.push(Entry::Properties(PropertyDelta {
            layer_id,
            before,
            after,
        }));
    }

    #[cfg(test)]
    pub fn record_node_properties(
        &mut self,
        node_id: u32,
        before: LayerProperties,
        after: LayerProperties,
    ) {
        self.record_properties(node_id, before, after);
    }

    fn push(&mut self, entry: Entry) {
        self.redo.clear();
        self.undo.push(entry);
        self.recompute_bytes();
        self.evict_oldest();
    }

    fn recompute_bytes(&mut self) {
        self.used_bytes = self
            .undo
            .iter()
            .chain(self.redo.iter())
            .map(Entry::storage_bytes)
            .fold(0usize, usize::saturating_add);
    }

    fn evict_oldest(&mut self) {
        while self.undo.len() > MAX_HISTORY || self.used_bytes > MAX_HISTORY_BYTES {
            if self.undo.is_empty() {
                break;
            }
            self.undo.remove(0);
            self.recompute_bytes();
        }
    }

    fn apply_pixels(doc: &mut Document, delta: &Delta, undo: bool) -> bool {
        let size = doc.size;
        if !delta.mask && undo && !delta.channel_was_present {
            let Some(layer) = doc.paint_layer_mut(delta.layer_id) else {
                return false;
            };
            if delta.patches.iter().any(|patch| {
                patch
                    .offset
                    .checked_add((patch.rows.saturating_sub(1)).saturating_mul(patch.stride))
                    .and_then(|start| start.checked_add(patch.row_bytes))
                    .is_none_or(|end| end > (size as usize * size as usize * 4))
                    || patch.row_bytes.saturating_mul(patch.rows) != patch.before.len()
                    || patch.before.len() != patch.after.len()
            }) {
                return false;
            }
            layer.pixels.remove(&delta.channel);
            return true;
        }
        let Some(target) =
            stroke_image_bytes_mut(doc, delta.layer_id, delta.mask, delta.channel, size)
        else {
            return false;
        };
        if delta.patches.iter().any(|patch| {
            patch
                .offset
                .checked_add((patch.rows.saturating_sub(1)).saturating_mul(patch.stride))
                .and_then(|start| start.checked_add(patch.row_bytes))
                .is_none_or(|end| end > target.len())
                || patch.row_bytes.saturating_mul(patch.rows) != patch.before.len()
                || patch.before.len() != patch.after.len()
        }) {
            return false;
        }
        for patch in &delta.patches {
            let source = if undo { &patch.before } else { &patch.after };
            for row in 0..patch.rows {
                let target_start = patch.offset + row * patch.stride;
                let source_start = row * patch.row_bytes;
                target[target_start..target_start + patch.row_bytes]
                    .copy_from_slice(&source[source_start..source_start + patch.row_bytes]);
            }
        }
        true
    }

    fn apply_uniform(doc: &mut Document, delta: &mut UniformDelta, undo: bool) -> bool {
        let size = doc.size;
        let Some(node) = doc.layer_tree.find_node_mut(delta.layer_id) else {
            return false;
        };
        let bytes_per_pixel = match delta.target {
            UniformTarget::Channel(_) => 4usize,
            UniformTarget::Mask => 1usize,
        };
        let Some(expected_len) = (size as usize)
            .checked_mul(size as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        else {
            return false;
        };
        if delta.inactive_present != delta.inactive.is_some()
            || delta
                .inactive
                .as_ref()
                .is_some_and(|bytes| bytes.len() != expected_len)
            || delta.inactive_is_after == undo
        {
            return false;
        }

        let current = match delta.target {
            UniformTarget::Channel(channel) => match &*node {
                LayerNode::Paint(layer) => layer
                    .pixels
                    .get(&channel)
                    .filter(|image| image.dimensions() == (size, size))
                    .map(|image| image.as_raw().as_slice()),
                LayerNode::Fill(_) | LayerNode::Group(_) => return false,
            },
            UniformTarget::Mask => match &*node {
                LayerNode::Paint(layer) if layer.mask.dimensions() == (size, size) => {
                    Some(layer.mask.as_raw().as_slice())
                }
                LayerNode::Fill(fill) if fill.properties.mask.dimensions() == (size, size) => {
                    Some(fill.properties.mask.as_raw().as_slice())
                }
                LayerNode::Group(group) if group.properties.mask.dimensions() == (size, size) => {
                    Some(group.properties.mask.as_raw().as_slice())
                }
                _ => return false,
            },
        };
        let current_present = current.is_some();
        let current_matches_value =
            current.is_some_and(|bytes| uniform_value_matches(bytes, delta.value));
        let current_matches_before = uniform_state_hash(current) == delta.before_hash;
        let current_exists_before = current_present == delta.before_present;
        if undo {
            if !current_matches_value {
                return false;
            }
        } else if !current_exists_before || !current_matches_before {
            return false;
        }

        match delta.target {
            UniformTarget::Channel(channel) => {
                let LayerNode::Paint(layer) = node else {
                    return false;
                };
                if delta.inactive_present
                    && !current_present
                    && layer.pixels.try_reserve(1).is_err()
                {
                    return false;
                }
                let mut current = layer.pixels.remove(&channel).map(RgbaImage::into_raw);
                let mut current_present = current.is_some();
                std::mem::swap(&mut delta.inactive, &mut current);
                std::mem::swap(&mut delta.inactive_present, &mut current_present);
                if let Some(bytes) = current {
                    let Some(image) = RgbaImage::from_raw(size, size, bytes) else {
                        return false;
                    };
                    layer.pixels.insert(channel, image);
                }
            }
            UniformTarget::Mask => {
                let mask = match node {
                    LayerNode::Paint(layer) => &mut layer.mask,
                    LayerNode::Fill(fill) => &mut fill.properties.mask,
                    LayerNode::Group(group) => &mut group.properties.mask,
                };
                let mut current = Some(std::mem::take(mask).into_raw());
                let mut current_present = true;
                std::mem::swap(&mut delta.inactive, &mut current);
                std::mem::swap(&mut delta.inactive_present, &mut current_present);
                let Some(bytes) = current else {
                    return false;
                };
                let Some(restored_mask) = GrayImage::from_raw(size, size, bytes) else {
                    return false;
                };
                *mask = restored_mask;
            }
        }
        // After undo, the parked buffer holds the edited (after) image; after
        // redo, it holds the original (before) image again.
        delta.inactive_is_after = undo;
        true
    }

    fn apply_mask_invert(doc: &mut Document, layer_id: u32) -> bool {
        let Some(node) = doc.layer_tree.find_node_mut(layer_id) else {
            return false;
        };
        let mask = match node {
            LayerNode::Paint(layer) => &mut layer.mask,
            LayerNode::Fill(fill) => &mut fill.properties.mask,
            LayerNode::Group(group) => &mut group.properties.mask,
        };
        if mask.dimensions() != (doc.size, doc.size) {
            return false;
        }
        for pixel in mask.as_mut() {
            *pixel = 255 - *pixel;
        }
        true
    }

    fn apply_tree_structure(
        doc: &mut Document,
        delta: &mut TreeStructureDelta,
        undo: bool,
    ) -> bool {
        match &mut delta.change {
            TreeStructureChange::Inserted { payload } => {
                if undo {
                    let Some(current) = doc.layer_tree.find_node(delta.node_id) else {
                        return false;
                    };
                    if doc.layer_tree.parent_id(delta.node_id) != Some(delta.parent_id)
                        || doc
                            .layer_tree
                            .siblings(delta.parent_id)
                            .ok()
                            .and_then(|nodes| nodes.get(delta.index))
                            .map(LayerNode::id)
                            != Some(delta.node_id)
                        || doc.layer_tree.selected_node_id != delta.selected_after_id
                        || subtree_contains_id(current, delta.selected_before_id)
                        || !history_payload_within_limit(node_storage_bytes(current))
                        || tree_extension_storage_bytes(&doc.layer_tree)
                            > MAX_TREE_EXTENSION_STORAGE_BYTES
                    {
                        return false;
                    }
                    let Ok(removed) = doc
                        .layer_tree
                        .remove(delta.node_id, delta.selected_before_id)
                    else {
                        return false;
                    };
                    if node_storage_bytes(&removed) > MAX_HISTORY_BYTES {
                        return false;
                    }
                    *payload = Some(removed);
                    true
                } else {
                    if doc.layer_tree.find_node(delta.node_id).is_some()
                        || doc.layer_tree.selected_node_id != delta.selected_before_id
                    {
                        return false;
                    }
                    let Some(stored) = payload.as_ref() else {
                        return false;
                    };
                    if node_storage_bytes(stored) > MAX_HISTORY_BYTES
                        || tree_extension_storage_bytes(&doc.layer_tree)
                            .saturating_add(subtree_extension_storage_bytes(stored))
                            > MAX_TREE_EXTENSION_STORAGE_BYTES
                        || (delta.selected_after_id != stored.id()
                            && !subtree_contains_id(stored, delta.selected_after_id)
                            && doc.layer_tree.find_node(delta.selected_after_id).is_none())
                        || decoded_bytes_for_node(stored)
                            .and_then(|bytes| doc.decoded_bytes().checked_add(bytes))
                            .is_none_or(|bytes| bytes > crate::document::MAX_DECODED_BYTES)
                    {
                        return false;
                    }
                    let Ok(copy) = clone_node_fallible(stored) else {
                        return false;
                    };
                    if doc
                        .layer_tree
                        .insert(delta.parent_id, delta.index, copy)
                        .is_err()
                        || !set_selection(doc, delta.selected_after_id)
                    {
                        return false;
                    }
                    *payload = None;
                    true
                }
            }
            TreeStructureChange::Removed { payload } => {
                if undo {
                    if doc.layer_tree.find_node(delta.node_id).is_some()
                        || doc.layer_tree.selected_node_id != delta.selected_after_id
                    {
                        return false;
                    }
                    let Some(stored) = payload.as_ref() else {
                        return false;
                    };
                    if node_storage_bytes(stored) > MAX_HISTORY_BYTES
                        || tree_extension_storage_bytes(&doc.layer_tree)
                            .saturating_add(subtree_extension_storage_bytes(stored))
                            > MAX_TREE_EXTENSION_STORAGE_BYTES
                        || (!subtree_contains_id(stored, delta.selected_before_id)
                            && doc.layer_tree.find_node(delta.selected_before_id).is_none())
                        || decoded_bytes_for_node(stored)
                            .and_then(|bytes| doc.decoded_bytes().checked_add(bytes))
                            .is_none_or(|bytes| bytes > crate::document::MAX_DECODED_BYTES)
                    {
                        return false;
                    }
                    let Ok(copy) = clone_node_fallible(stored) else {
                        return false;
                    };
                    if doc
                        .layer_tree
                        .insert(delta.parent_id, delta.index, copy)
                        .is_err()
                        || !set_selection(doc, delta.selected_before_id)
                    {
                        return false;
                    }
                    *payload = None;
                    true
                } else {
                    if payload.is_some()
                        || doc.layer_tree.parent_id(delta.node_id) != Some(delta.parent_id)
                        || doc
                            .layer_tree
                            .siblings(delta.parent_id)
                            .ok()
                            .and_then(|nodes| nodes.get(delta.index))
                            .map(LayerNode::id)
                            != Some(delta.node_id)
                        || doc.layer_tree.selected_node_id != delta.selected_before_id
                    {
                        return false;
                    }
                    let Some(node) = doc.layer_tree.find_node(delta.node_id) else {
                        return false;
                    };
                    if node_storage_bytes(node) > MAX_HISTORY_BYTES
                        || tree_extension_storage_bytes(&doc.layer_tree)
                            > MAX_TREE_EXTENSION_STORAGE_BYTES
                        || subtree_contains_id(node, delta.selected_after_id)
                    {
                        return false;
                    }
                    let Ok(removed) = doc
                        .layer_tree
                        .remove(delta.node_id, delta.selected_after_id)
                    else {
                        return false;
                    };
                    *payload = Some(removed);
                    true
                }
            }
            TreeStructureChange::Moved {
                from_parent_id,
                from_index,
                to_parent_id,
                to_index,
            } => {
                let (expected_parent, expected_index, target_parent, target_index) = if undo {
                    (*to_parent_id, *to_index, *from_parent_id, *from_index)
                } else {
                    (*from_parent_id, *from_index, *to_parent_id, *to_index)
                };
                if doc.layer_tree.parent_id(delta.node_id) != Some(expected_parent)
                    || doc
                        .layer_tree
                        .siblings(expected_parent)
                        .ok()
                        .and_then(|nodes| nodes.get(expected_index))
                        .map(LayerNode::id)
                        != Some(delta.node_id)
                    || doc.layer_tree.selected_node_id != delta.selected_before_id
                    || tree_extension_storage_bytes(&doc.layer_tree)
                        > MAX_TREE_EXTENSION_STORAGE_BYTES
                {
                    return false;
                }
                doc.layer_tree
                    .move_node(delta.node_id, target_parent, target_index)
                    .is_ok()
            }
        }
    }

    fn apply_stack(doc: &mut Document, delta: &mut StackDelta, undo: bool) -> bool {
        match &mut delta.change {
            StackChange::Added {
                name,
                duplicate_source,
                payload,
            } => {
                if undo {
                    let Some(position) = doc
                        .layer_tree
                        .roots
                        .iter()
                        .position(|node| node.id() == delta.layer_id)
                    else {
                        return false;
                    };
                    if doc.layer_tree.roots.len() <= 1
                        || doc.layer_tree.find_node(delta.selected_before).is_none()
                        || !matches!(doc.layer_tree.roots[position], LayerNode::Paint(_))
                    {
                        return false;
                    }
                    let Ok(removed) = doc.layer_tree.remove(delta.layer_id, delta.selected_before)
                    else {
                        return false;
                    };
                    if let LayerNode::Paint(removed) = removed {
                        if payload.is_some() {
                            *payload = Some(removed);
                        }
                    } else {
                        return false;
                    }
                    set_selection(doc, delta.selected_before)
                } else {
                    if doc.layer_tree.find_node(delta.layer_id).is_some()
                        || doc.layer_tree.roots.len() >= crate::layer_tree::MAX_LAYER_TREE_NODES
                        || (delta.selected_after != delta.layer_id
                            && doc.layer_tree.find_node(delta.selected_after).is_none())
                    {
                        return false;
                    }
                    let layer = if let Some(layer) = payload.take() {
                        if doc
                            .decoded_bytes()
                            .checked_add(layer_image_bytes(&layer))
                            .is_none_or(|bytes| bytes > crate::document::MAX_DECODED_BYTES)
                        {
                            *payload = Some(layer);
                            return false;
                        }
                        layer
                    } else if let Some(source_id) = duplicate_source {
                        let Some(source) = doc.paint_layer(*source_id) else {
                            return false;
                        };
                        let projected = doc.decoded_bytes().checked_add(layer_image_bytes(source));
                        if projected.is_none_or(|bytes| bytes > crate::document::MAX_DECODED_BYTES)
                        {
                            return false;
                        }
                        let mut duplicate = source.clone();
                        duplicate.id = delta.layer_id;
                        duplicate.name.clone_from(name);
                        duplicate
                    } else {
                        let layer_bytes = u64::from(doc.size) * u64::from(doc.size);
                        if doc
                            .decoded_bytes()
                            .checked_add(layer_bytes)
                            .is_none_or(|bytes| bytes > crate::document::MAX_DECODED_BYTES)
                        {
                            return false;
                        }
                        Layer::new(delta.layer_id, name.clone(), doc.size)
                    };
                    let index = delta.index.min(doc.layer_tree.roots.len());
                    if doc
                        .layer_tree
                        .insert(None, index, LayerNode::Paint(layer))
                        .is_err()
                    {
                        return false;
                    }
                    set_selection(doc, delta.selected_after)
                }
            }
            StackChange::Removed { payload } => {
                if undo {
                    if payload.is_none() {
                        return false;
                    }
                    if doc.layer_tree.find_node(delta.layer_id).is_some()
                        || doc.layer_tree.roots.len() >= crate::layer_tree::MAX_LAYER_TREE_NODES
                        || (delta.selected_before != delta.layer_id
                            && doc.layer_tree.find_node(delta.selected_before).is_none())
                    {
                        return false;
                    }
                    let layer = payload.take().expect("payload checked above");
                    let index = delta.index.min(doc.layer_tree.roots.len());
                    if doc
                        .layer_tree
                        .insert(None, index, LayerNode::Paint(layer))
                        .is_err()
                    {
                        return false;
                    }
                    set_selection(doc, delta.selected_before)
                } else {
                    let Some(position) = doc
                        .layer_tree
                        .roots
                        .iter()
                        .position(|node| node.id() == delta.layer_id)
                    else {
                        return false;
                    };
                    if doc.layer_tree.roots.len() <= 1
                        || payload.is_some()
                        || !matches!(doc.layer_tree.roots[position], LayerNode::Paint(_))
                    {
                        return false;
                    }
                    if doc.layer_tree.find_node(delta.selected_after).is_none() {
                        return false;
                    }
                    let Ok(removed) = doc.layer_tree.remove(delta.layer_id, delta.selected_after)
                    else {
                        return false;
                    };
                    let LayerNode::Paint(layer) = removed else {
                        return false;
                    };
                    *payload = Some(layer);
                    true
                }
            }
            StackChange::Moved {
                old_index,
                new_index,
            } => {
                let Some(position) = doc
                    .layer_tree
                    .roots
                    .iter()
                    .position(|node| node.id() == delta.layer_id)
                else {
                    return false;
                };
                let target = if undo { *old_index } else { *new_index };
                let selected_id = if undo {
                    delta.selected_before
                } else {
                    delta.selected_after
                };
                if target >= doc.layer_tree.roots.len()
                    || doc.layer_tree.find_node(selected_id).is_none()
                {
                    return false;
                }
                let current_parent = doc.layer_tree.parent_id(delta.layer_id);
                if current_parent != Some(None)
                    || position != if undo { *new_index } else { *old_index }
                {
                    return false;
                }
                doc.layer_tree
                    .move_node(delta.layer_id, None, target)
                    .is_ok()
            }
        }
    }

    fn apply(doc: &mut Document, entry: &mut Entry, undo: bool) -> bool {
        match entry {
            Entry::Pixels(delta) => Self::apply_pixels(doc, delta, undo),
            Entry::Uniform(delta) => Self::apply_uniform(doc, delta, undo),
            Entry::MaskInvert { layer_id } => Self::apply_mask_invert(doc, *layer_id),
            Entry::Stack(delta) => Self::apply_stack(doc, delta, undo),
            Entry::TreeStructure(delta) => Self::apply_tree_structure(doc, delta, undo),
            Entry::FillValue(delta) => {
                let Some(LayerNode::Fill(fill)) = doc.layer_tree.find_node_mut(delta.node_id)
                else {
                    return false;
                };
                let (source, target) = if undo {
                    (delta.after, delta.before)
                } else {
                    (delta.before, delta.after)
                };
                if fill.values.get(&delta.channel).copied() != source {
                    return false;
                }
                if let Some(value) = target {
                    if !fill.values.contains_key(&delta.channel)
                        && fill.values.try_reserve(1).is_err()
                    {
                        return false;
                    }
                    fill.values.insert(delta.channel, value);
                } else {
                    fill.values.remove(&delta.channel);
                }
                true
            }
            Entry::Mesh {
                mesh,
                material_bindings,
                ..
            } => {
                if !mesh_state_is_valid(doc, mesh, material_bindings)
                    || !mesh_state_is_valid(doc, &doc.mesh, &doc.material_bindings)
                {
                    return false;
                }
                std::mem::swap(&mut doc.mesh, mesh);
                std::mem::swap(&mut doc.material_bindings, material_bindings);
                true
            }
            Entry::Properties(delta) => {
                let Some(node) = doc.layer_tree.find_node_mut(delta.layer_id) else {
                    return false;
                };
                let expected = if undo { &delta.after } else { &delta.before };
                if LayerProperties::capture_node(node) != *expected {
                    return false;
                }
                if undo {
                    delta.before.apply_node(node);
                } else {
                    delta.after.apply_node(node);
                }
                true
            }
        }
    }

    pub fn undo(&mut self, doc: &mut Document) -> bool {
        let Some(mut entry) = self.undo.pop() else {
            return false;
        };
        if Self::apply(doc, &mut entry, true) {
            self.redo.push(entry);
            self.recompute_bytes();
            true
        } else {
            self.undo.push(entry);
            false
        }
    }
    pub fn redo(&mut self, doc: &mut Document) -> bool {
        let Some(mut entry) = self.redo.pop() else {
            return false;
        };
        if Self::apply(doc, &mut entry, false) {
            self.undo.push(entry);
            self.recompute_bytes();
            true
        } else {
            self.redo.push(entry);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn paint_root(doc: &Document, index: usize) -> &Layer {
        match &doc.layer_tree.roots[index] {
            LayerNode::Paint(layer) => layer,
            _ => panic!("expected root Paint layer at index {index}"),
        }
    }

    fn paint_root_mut(doc: &mut Document, index: usize) -> &mut Layer {
        match &mut doc.layer_tree.roots[index] {
            LayerNode::Paint(layer) => layer,
            _ => panic!("expected root Paint layer at index {index}"),
        }
    }

    fn selected_root_index(doc: &Document) -> usize {
        doc.layer_tree
            .roots
            .iter()
            .position(|node| node.id() == doc.layer_tree.selected_node_id)
            .expect("selected test node is a root")
    }

    fn paint_node(id: u32, size: u32) -> LayerNode {
        LayerNode::Paint(Layer::new(id, format!("Paint {id}"), size))
    }

    fn group_node(id: u32, size: u32, children: Vec<LayerNode>) -> LayerNode {
        LayerNode::Group(LayerGroup {
            properties: LayerNodeProperties::new(id, format!("Group {id}"), size).unwrap(),
            children,
        })
    }

    fn named_mesh(name: &str, material_count: usize) -> Mesh {
        let mut mesh = Mesh::demo_cube();
        mesh.name = name.into();
        mesh.materials = (0..material_count)
            .map(|index| format!("{name} material {index}"))
            .collect();
        mesh.triangle_materials.fill(0);
        mesh
    }

    #[test]
    fn mesh_history_budget_accounts_for_additional_uv_sets() {
        let mesh = Mesh::demo_cube();
        let base_bytes = mesh_storage_bytes(&mesh, 0);
        let mut with_extra_uv = mesh;
        let vertex_count = with_extra_uv.vertices.len();
        with_extra_uv
            .extra_uv_sets
            .insert(1, vec![Some([0.25, 0.75]); vertex_count]);
        assert!(
            mesh_storage_bytes(&with_extra_uv, 0)
                >= base_bytes + std::mem::size_of::<Option<[f32; 2]>>() * vertex_count
        );
    }

    fn replace_mesh_for_test(doc: &mut Document, history: &mut History, mesh: Mesh) -> bool {
        let bindings = vec![Some(doc.active_set_id); mesh.material_count()];
        let previous_mesh = std::mem::replace(&mut doc.mesh, mesh);
        let previous_bindings = std::mem::replace(&mut doc.material_bindings, bindings);
        history.record_mesh_replacement(previous_mesh, previous_bindings)
    }

    fn encoded_mesh(mesh: &Mesh) -> Vec<u8> {
        serde_json::to_vec(mesh).unwrap()
    }

    #[test]
    fn mesh_replacement_history_undoes_and_redoes_exact_mesh_and_bindings() {
        let mut doc = Document::new(64);
        let old_mesh = encoded_mesh(&doc.mesh);
        let old_bindings = doc.material_bindings.clone();
        let mut history = History::new();

        assert!(replace_mesh_for_test(
            &mut doc,
            &mut history,
            named_mesh("Imported mesh", 2),
        ));
        let imported_mesh = encoded_mesh(&doc.mesh);
        let imported_bindings = doc.material_bindings.clone();
        assert_ne!(imported_mesh, old_mesh);
        assert_eq!(
            history
                .mesh_recovery_target(HistoryDirection::Undo)
                .unwrap()
                .1,
            old_bindings
        );

        assert!(history.undo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), old_mesh);
        assert_eq!(doc.material_bindings, old_bindings);
        assert_eq!(
            history
                .mesh_recovery_target(HistoryDirection::Redo)
                .unwrap()
                .1,
            imported_bindings
        );

        assert!(history.redo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), imported_mesh);
        assert_eq!(doc.material_bindings, imported_bindings);
    }

    #[test]
    fn mesh_replacement_history_supports_multiple_swaps_in_order() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let original = encoded_mesh(&doc.mesh);
        assert!(replace_mesh_for_test(
            &mut doc,
            &mut history,
            named_mesh("Second", 1),
        ));
        let second = encoded_mesh(&doc.mesh);
        assert!(replace_mesh_for_test(
            &mut doc,
            &mut history,
            named_mesh("Third", 3),
        ));
        let third = encoded_mesh(&doc.mesh);

        assert!(history.undo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), second);
        assert!(history.undo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), original);
        assert!(history.redo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), second);
        assert!(history.redo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), third);
    }

    #[test]
    fn invalid_mesh_history_input_preserves_older_entries_and_apply_is_atomic() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        assert!(replace_mesh_for_test(
            &mut doc,
            &mut history,
            named_mesh("Current", 1),
        ));
        let retained_bytes = history.used_bytes;
        let invalid_mesh = named_mesh("Invalid binding count", 2);
        assert!(!history.record_mesh_replacement(invalid_mesh, vec![Some(1)]));
        assert!(history.can_undo());
        assert_eq!(history.used_bytes, retained_bytes);

        // A nonzero but unavailable set ID cannot be checked by the recorder,
        // which has no Document reference. The live swap validates it before
        // touching either the mesh or its binding vector.
        assert!(
            history.record_mesh_replacement(named_mesh("Missing set target", 1), vec![Some(999)],)
        );
        let current = encoded_mesh(&doc.mesh);
        let current_bindings = doc.material_bindings.clone();
        assert!(!history.undo(&mut doc));
        assert_eq!(encoded_mesh(&doc.mesh), current);
        assert_eq!(doc.material_bindings, current_bindings);
    }

    #[test]
    fn new_mesh_replacement_invalidates_redo_branch() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        assert!(replace_mesh_for_test(
            &mut doc,
            &mut history,
            named_mesh("First import", 1),
        ));
        assert!(history.undo(&mut doc));
        assert!(history.can_redo());
        assert!(replace_mesh_for_test(
            &mut doc,
            &mut history,
            named_mesh("Branch import", 2),
        ));
        assert!(!history.can_redo());
    }

    #[test]
    fn sparse_undo_redo_reproduces_pixels() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(1, 1, Rgba([20, 40, 60, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        assert!(history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        }));
        assert!(history.undo(&mut doc));
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
                .get_pixel(1, 1)
                .0,
            [20, 40, 60, 255]
        );
        assert!(history.used_bytes <= 8192 + std::mem::size_of::<Patch>());
    }

    #[test]
    fn nested_paint_pixels_and_mask_undo_redo_by_stable_node_id() {
        let mut doc = Document::new(64);
        doc.layer_tree
            .insert(None, 1, group_node(2, 64, vec![paint_node(3, 64)]))
            .unwrap();
        doc.layer_tree.select(3).unwrap();
        let mut history = History::new();

        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.paint_layer_mut(3)
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(8, 9, Rgba([31, 41, 59, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        assert!(history.commit(Snapshot {
            layer_id: 3,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        }));
        assert!(history.undo(&mut doc));
        assert!(!doc
            .paint_layer(3)
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.paint_layer(3).unwrap().pixels[&Channel::BaseColor]
                .get_pixel(8, 9)
                .0,
            [31, 41, 59, 255]
        );

        let before = History::capture(&doc, Channel::BaseColor, BrushMode::MaskBlack);
        doc.paint_layer_mut(3)
            .unwrap()
            .mask
            .put_pixel(8, 9, image::Luma([73]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::MaskBlack);
        assert!(history.commit(Snapshot {
            layer_id: 3,
            channel: Channel::BaseColor,
            mask: true,
            channel_was_present: true,
            before,
            after,
        }));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.paint_layer(3).unwrap().mask.get_pixel(8, 9).0, [255]);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.paint_layer(3).unwrap().mask.get_pixel(8, 9).0, [73]);
    }

    #[test]
    fn fill_value_delta_and_group_property_delta_round_trip() {
        let mut doc = Document::new(64);
        let fill = LayerNode::Fill(FillLayer {
            properties: LayerNodeProperties::new(2, "Fill", 64).unwrap(),
            values: HashMap::from([(Channel::BaseColor, [1, 2, 3, 255])]),
        });
        doc.layer_tree
            .insert(None, 1, group_node(3, 64, vec![fill]))
            .unwrap();
        let mut history = History::new();

        let before = [1, 2, 3, 255];
        let after = [90, 80, 70, 255];
        let LayerNode::Fill(fill) = doc.layer_tree.find_node_mut(2).unwrap() else {
            panic!("expected Fill node")
        };
        fill.values.insert(Channel::BaseColor, after);
        history.record_fill_value(2, Channel::BaseColor, Some(before), Some(after));
        let events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [DocumentEvent::LayerTreeFillValueSet {
                node_id: 2,
                value: Some([1, 2, 3, 255]),
                ..
            }]
        ));
        assert!(history.undo(&mut doc));
        let LayerNode::Fill(fill) = doc.layer_tree.find_node(2).unwrap() else {
            panic!("expected Fill node")
        };
        assert_eq!(fill.values.get(&Channel::BaseColor), Some(&before));
        assert!(history.redo(&mut doc));
        let LayerNode::Fill(fill) = doc.layer_tree.find_node(2).unwrap() else {
            panic!("expected Fill node")
        };
        assert_eq!(fill.values.get(&Channel::BaseColor), Some(&after));

        let before = LayerProperties::capture_node(doc.layer_tree.find_node(3).unwrap());
        let LayerNode::Group(group) = doc.layer_tree.find_node_mut(3).unwrap() else {
            panic!("expected Group node")
        };
        group.properties.name = "Changed Group".into();
        group.properties.opacity = 0.4;
        let after = LayerProperties::capture_node(doc.layer_tree.find_node(3).unwrap());
        history.record_node_properties(3, before, after);
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.find_node(3).unwrap().name(), "Group 3");
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layer_tree.find_node(3).unwrap().name(), "Changed Group");
    }

    #[test]
    fn subtree_insert_undo_redo_preserves_structure_and_selection() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let subtree = group_node(
            2,
            64,
            vec![
                LayerNode::Fill(FillLayer {
                    properties: LayerNodeProperties::new(3, "Fill", 64).unwrap(),
                    values: HashMap::from([(Channel::Roughness, [128; 4])]),
                }),
                paint_node(4, 64),
            ],
        );
        history
            .insert_subtree(&mut doc, None, 1, subtree, 4)
            .unwrap();
        assert_eq!(doc.layer_tree.selected_node_id, 4);
        assert_eq!(doc.layer_tree.parent_id(4), Some(Some(2)));

        doc.layer_tree.select(1).unwrap();
        let roots_before_failed_undo = doc.layer_tree.roots.len();
        let history_bytes_before_failed_undo = history.used_bytes;
        assert!(!history.undo(&mut doc));
        assert_eq!(doc.layer_tree.selected_node_id, 1);
        assert_eq!(doc.layer_tree.roots.len(), roots_before_failed_undo);
        assert!(doc.layer_tree.find_node(2).is_some());
        assert!(history.can_undo());
        assert_eq!(history.used_bytes, history_bytes_before_failed_undo);
        doc.layer_tree.select(4).unwrap();

        let undo_events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            undo_events.as_slice(),
            [DocumentEvent::LayerTreeNodeRemove {
                node_id: 2,
                selected_after_node_id: 1,
                ..
            }]
        ));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.selected_node_id, 1);
        assert!(doc.layer_tree.find_node(2).is_none());

        let redo_events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Redo)
            .unwrap();
        assert!(redo_events.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerTreeSubtreeBegin {
                root_node_id: 2,
                selected_node_id: 4,
                ..
            }
        )));
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layer_tree.selected_node_id, 4);
        assert_eq!(doc.layer_tree.parent_id(4), Some(Some(2)));
        let LayerNode::Group(group) = doc.layer_tree.find_node(2).unwrap() else {
            panic!("expected restored Group")
        };
        assert!(matches!(group.children[0], LayerNode::Fill(_)));
        assert!(matches!(group.children[1], LayerNode::Paint(_)));
    }

    #[test]
    fn duplicate_nested_group_rekeys_subtree_and_preserves_external_graph_sources() {
        let mut doc = Document::new(64);
        let mut graph_source = Layer::new(4, "Graph source", 64);
        graph_source.procedural_graph =
            Some(crate::procedural::ProceduralGraph::value_noise_times_occlusion(7, 8.0, 6));
        graph_source.mask.put_pixel(3, 5, image::Luma([91]));
        graph_source.pixels_mut(Channel::Roughness, 64).put_pixel(
            3,
            5,
            image::Rgba([31, 0, 0, 255]),
        );
        graph_source
            .extensions
            .insert("futurePaint".into(), serde_json::json!({"kept": true}));

        let mut external_graph_source = Layer::new(6, "External graph", 64);
        external_graph_source.procedural_graph =
            Some(crate::procedural::ProceduralGraph::value_noise_times_occlusion(9, 4.0, 1));
        external_graph_source.visible = false;
        external_graph_source
            .mask
            .put_pixel(9, 10, image::Luma([44]));
        let mut nested_properties = LayerNodeProperties::new(5, "Nested group", 64).unwrap();
        nested_properties.opacity = 0.4;
        nested_properties.mask.put_pixel(2, 4, image::Luma([73]));
        let nested = LayerNode::Group(LayerGroup {
            properties: nested_properties,
            children: vec![LayerNode::Paint(external_graph_source)],
        });
        let mut fill_properties = LayerNodeProperties::new(3, "Fill", 64).unwrap();
        fill_properties.mask.put_pixel(4, 6, image::Luma([123]));
        fill_properties.extensions.insert(
            "futureFill".into(),
            serde_json::json!({"preserve": [1, 2, 3]}),
        );
        let fill = LayerNode::Fill(FillLayer {
            properties: fill_properties,
            values: HashMap::from([(Channel::BaseColor, [11, 22, 33, 255])]),
        });
        let mut group_properties = LayerNodeProperties::new(2, "Material group", 64).unwrap();
        group_properties.visible = false;
        group_properties.opacity = 0.625;
        group_properties.mask.put_pixel(1, 7, image::Luma([166]));
        group_properties.disabled_channels.insert(Channel::Metallic);
        let source_group = LayerNode::Group(LayerGroup {
            properties: group_properties,
            children: vec![fill, LayerNode::Paint(graph_source), nested],
        });
        doc.layer_tree.insert(None, 1, source_group).unwrap();
        doc.select_node(2).unwrap();
        let source_before = doc.layer_tree.find_node(2).unwrap().clone();
        let mut saved = Document::new(64);
        saved.layer_tree = doc.layer_tree.clone();
        let mut history = History::new();

        let duplicate_id = history.duplicate_subtree(&mut doc, 2).unwrap();
        assert_eq!(duplicate_id, 7);
        assert_eq!(doc.layer_tree.selected_node_id, duplicate_id);
        assert_eq!(doc.layer_tree.parent_id(duplicate_id), Some(None));
        assert_eq!(doc.layer_tree.roots[2].id(), duplicate_id);
        assert_eq!(doc.layer_tree.next_node_id, 12);
        let LayerNode::Group(copy) = doc.layer_tree.find_node(duplicate_id).unwrap() else {
            panic!("duplicate should retain its Group type");
        };
        assert_eq!(copy.properties.name, "Material group copy");
        assert!(!copy.properties.visible);
        assert_eq!(copy.properties.opacity, 0.625);
        assert_eq!(copy.properties.mask.get_pixel(1, 7).0, [166]);
        assert!(copy
            .properties
            .disabled_channels
            .contains(&Channel::Metallic));
        assert_eq!(
            copy.children.iter().map(LayerNode::id).collect::<Vec<_>>(),
            [8, 9, 10]
        );
        let LayerNode::Fill(fill_copy) = &copy.children[0] else {
            panic!("first copied child should remain a Fill");
        };
        assert_eq!(fill_copy.values[&Channel::BaseColor], [11, 22, 33, 255]);
        assert_eq!(fill_copy.properties.mask.get_pixel(4, 6).0, [123]);
        assert_eq!(
            fill_copy.properties.extensions["futureFill"],
            serde_json::json!({"preserve": [1, 2, 3]})
        );
        let LayerNode::Paint(graph_copy) = &copy.children[1] else {
            panic!("second copied child should remain Paint");
        };
        assert_eq!(graph_copy.name, "Graph source");
        assert_eq!(graph_copy.mask.get_pixel(3, 5).0, [91]);
        assert_eq!(
            graph_copy.pixels[&Channel::Roughness].get_pixel(3, 5).0,
            [31, 0, 0, 255]
        );
        assert_eq!(
            graph_copy.extensions["futurePaint"],
            serde_json::json!({"kept": true})
        );
        assert_eq!(
            graph_copy
                .procedural_graph
                .as_ref()
                .unwrap()
                .occlusion_source_layer_id(),
            Some(11)
        );
        let LayerNode::Group(nested_copy) = &copy.children[2] else {
            panic!("third copied child should remain a Group");
        };
        assert_eq!(nested_copy.properties.id, 10);
        assert_eq!(nested_copy.properties.mask.get_pixel(2, 4).0, [73]);
        let LayerNode::Paint(external_reference_copy) = &nested_copy.children[0] else {
            panic!("nested Paint node should remain in the copied tree");
        };
        assert!(!external_reference_copy.visible);
        assert_eq!(external_reference_copy.mask.get_pixel(9, 10).0, [44]);
        assert_eq!(
            external_reference_copy
                .procedural_graph
                .as_ref()
                .unwrap()
                .occlusion_source_layer_id(),
            Some(1)
        );
        assert_eq!(
            doc.layer_tree.find_node(2).unwrap().name(),
            source_before.name()
        );

        let events = crate::recovery::prepare_layer_tree_subtree_events(
            doc.layer_tree.find_node(duplicate_id).unwrap(),
            doc.active_set_id,
            None,
            2,
            duplicate_id,
        )
        .unwrap();
        let mut replayer = crate::recovery::MeshRecoveryReplayer::new();
        for event in events {
            replayer.apply_event(&mut saved, event).unwrap();
        }
        assert_eq!(saved.layer_tree.roots[2].id(), duplicate_id);
        assert_eq!(saved.layer_tree.roots[2].name(), "Material group copy");
        assert_eq!(
            saved.layer_tree.all_node_ids(),
            doc.layer_tree.all_node_ids()
        );

        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.selected_node_id, 2);
        assert!(doc.layer_tree.find_node(duplicate_id).is_none());
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layer_tree.selected_node_id, duplicate_id);
        assert_eq!(
            doc.layer_tree.all_node_ids(),
            saved.layer_tree.all_node_ids()
        );
    }

    #[test]
    fn duplicate_subtree_rejects_paint_leaf_and_id_limits_atomically() {
        let mut doc = Document::new(64);
        for id in 2..=MAX_LAYERS as u32 {
            doc.layer_tree
                .roots
                .push(LayerNode::Paint(Layer::new(id, format!("Layer {id}"), 64)));
        }
        doc.layer_tree.next_node_id = MAX_LAYERS as u32 + 1;
        doc.layer_tree.selected_node_id = MAX_LAYERS as u32;
        let before_ids = doc.layer_tree.all_node_ids();
        let before_selection = doc.layer_tree.selected_node_id;
        let before_next_id = doc.layer_tree.next_node_id;
        let mut history = History::new();
        assert_eq!(
            history.duplicate_subtree(&mut doc, before_selection),
            Err("Duplicated subtree exceeds the Paint layer limit")
        );
        assert_eq!(doc.layer_tree.all_node_ids(), before_ids);
        assert_eq!(doc.layer_tree.selected_node_id, before_selection);
        assert_eq!(doc.layer_tree.next_node_id, before_next_id);
        assert!(!history.can_undo());

        let mut id_limited = Document::new(64);
        let selected = id_limited.layer_tree.selected_node_id;
        id_limited.layer_tree.next_node_id = u32::MAX;
        let before_ids = id_limited.layer_tree.all_node_ids();
        assert_eq!(
            history.duplicate_subtree(&mut id_limited, selected),
            Err("Layer tree node ID counter exhausted")
        );
        assert_eq!(id_limited.layer_tree.all_node_ids(), before_ids);
        assert_eq!(id_limited.layer_tree.selected_node_id, selected);
        assert_eq!(id_limited.layer_tree.next_node_id, u32::MAX);
        assert!(!history.can_undo());
    }

    #[test]
    fn over_budget_subtree_insert_preserves_document_and_history() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let mut oversized = Layer::new(2, "Oversized", 64);
        oversized.extensions.insert(
            "future".into(),
            serde_json::Value::String("x".repeat(MAX_HISTORY_BYTES + 1)),
        );
        let original_selection = doc.layer_tree.selected_node_id;
        let original_next_id = doc.layer_tree.next_node_id;
        let err = history
            .insert_subtree(&mut doc, None, 1, LayerNode::Paint(oversized), 2)
            .unwrap_err();
        assert!(err.contains("96-MiB"));
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(doc.layer_tree.selected_node_id, original_selection);
        assert_eq!(doc.layer_tree.next_node_id, original_next_id);
        assert!(history.undo.is_empty());
        assert!(history.redo.is_empty());
        assert_eq!(history.used_bytes, 0);
        assert!(history_payload_within_limit(MAX_HISTORY_BYTES));
        assert!(!history_payload_within_limit(MAX_HISTORY_BYTES + 1));
    }

    #[test]
    fn tree_subtree_payload_accounts_exact_history_boundary() {
        let size = 2048;
        let mut layer = Layer::new(2, "Boundary", size);
        for channel in [
            Channel::BaseColor,
            Channel::Roughness,
            Channel::Metallic,
            Channel::Normal,
            Channel::Height,
        ] {
            layer.pixels.insert(channel, RgbaImage::new(size, size));
        }
        layer
            .extensions
            .insert("padding".into(), serde_json::Value::String(String::new()));
        let node = LayerNode::Paint(layer);
        let base = node_storage_bytes(&node);
        assert!(base < MAX_HISTORY_BYTES);
        assert_eq!(
            match &node {
                LayerNode::Paint(layer) => layer
                    .extensions
                    .get("padding")
                    .and_then(serde_json::Value::as_str),
                _ => None,
            },
            Some("")
        );
        let padding_len = MAX_HISTORY_BYTES - base;
        let mut padding = String::new();
        padding.try_reserve_exact(padding_len).unwrap();
        padding.extend(std::iter::repeat_n('x', padding_len));
        let mut layer = match node {
            LayerNode::Paint(layer) => layer,
            _ => unreachable!(),
        };
        layer
            .extensions
            .insert("padding".into(), serde_json::Value::String(padding));
        let exact = LayerNode::Paint(layer);
        assert_eq!(node_storage_bytes(&exact), MAX_HISTORY_BYTES);
        assert!(History::can_retain_subtree(&exact));

        let LayerNode::Paint(layer) = exact else {
            unreachable!();
        };
        let mut over = layer;
        let mut padding = String::new();
        padding.try_reserve_exact(padding_len + 1).unwrap();
        padding.extend(std::iter::repeat_n('x', padding_len + 1));
        over.extensions
            .insert("padding".into(), serde_json::Value::String(padding));
        assert!(!History::can_retain_subtree(&LayerNode::Paint(over)));
    }

    #[test]
    fn snapshot_capture_is_read_only_and_absent_channel_undo_removes_redo_restores() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
        let mut history = History::new();
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));

        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        assert_eq!(before.len(), 64 * 64 * 4);
        assert!(before.iter().all(|byte| *byte == 0));
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));

        {
            let size = doc.size;
            let image = doc
                .selected_layer_mut()
                .unwrap()
                .pixels_mut(Channel::BaseColor, size);
            image.put_pixel(7, 9, Rgba([17, 31, 47, 255]));
            image.put_pixel(63, 63, Rgba([5, 11, 23, 255]));
        }
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        let expected_after = after.clone();
        history.commit(Snapshot {
            layer_id,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        });

        assert!(history.undo(&mut doc));
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor].as_raw(),
            &expected_after
        );

        let undo_events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(undo_events.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerChannelRemove {
                set_id: 1,
                layer_id: id,
                channel: Channel::BaseColor,
            } if *id == layer_id
        )));
        for event in undo_events {
            event.apply(&mut doc).unwrap();
        }
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
    }

    #[test]
    fn four_k_stroke_capture_is_sparse_and_repeated_tiles_are_captured_once() {
        let mut doc = Document::new(4096);
        let mut history = History::new();
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::Paint);
        let brush = crate::paint::Brush {
            radius: 2,
            hardness: 1.0,
            opacity: 1.0,
            color: [20, 40, 60, 255],
            ..crate::paint::Brush::default()
        };
        brush.stroke_masked_recording(
            &mut doc,
            Channel::BaseColor,
            Some([0.4, 0.4]),
            [0.401, 0.4],
            None,
            |doc, min_x, min_y, max_x, max_y| {
                History::capture_stroke_rect(&mut capture, doc, min_x, min_y, max_x, max_y)
            },
        );
        let captured_once = capture.projected_storage_bytes();
        History::capture_stroke_rect(&mut capture, &doc, 1638, 1638, 1641, 1641);
        assert_eq!(capture.projected_storage_bytes(), captured_once);
        assert!(captured_once < 40 * 1024);
        assert_ne!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
                .get_pixel(1638, 1638)
                .0,
            [0; 4]
        );
        assert_eq!(
            history.commit_stroke(capture, &mut doc),
            StrokeCommitResult::Committed
        );
        assert!(history.undo(&mut doc));
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
                .get_pixel(1638, 1638)
                .0,
            [20, 40, 60, 255]
        );
    }

    #[test]
    fn mask_stroke_tiles_undo_and_redo_exactly() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::MaskWhite);
        History::capture_stroke_rect(&mut capture, &doc, 7, 9, 8, 10);
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(7, 9, image::Luma([42]));
        assert_eq!(
            history.commit_stroke(capture, &mut doc),
            StrokeCommitResult::Committed
        );
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().unwrap().mask.get_pixel(7, 9).0, [255]);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.selected_layer().unwrap().mask.get_pixel(7, 9).0, [42]);
    }

    #[test]
    fn fill_and_group_mask_strokes_commit_undo_redo_and_recover_by_node_id() {
        for is_group in [false, true] {
            let mut doc = Document::new(64);
            let base_id = doc.layer_tree.selected_node_id;
            let id = doc.layer_tree.next_node_id;
            let properties = LayerNodeProperties::new(id, "Mask target", 64).unwrap();
            let node = if is_group {
                LayerNode::Group(LayerGroup {
                    properties,
                    children: Vec::new(),
                })
            } else {
                LayerNode::Fill(FillLayer {
                    properties,
                    values: [(Channel::BaseColor, [255; 4])].into_iter().collect(),
                })
            };
            doc.layer_tree.roots.push(node);
            doc.layer_tree.selected_node_id = id;
            doc.layer_tree.next_node_id = id + 1;
            let mut saved = Document::new(64);
            saved.layer_tree = doc.layer_tree.clone();
            let brush = crate::paint::Brush {
                radius: 2,
                hardness: 1.0,
                mode: BrushMode::MaskBlack,
                ..crate::paint::Brush::default()
            };
            let mut history = History::new();
            let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::MaskBlack);
            brush.stroke_masked_recording(
                &mut doc,
                Channel::BaseColor,
                None,
                [0.5, 0.5],
                None,
                |doc, min_x, min_y, max_x, max_y| {
                    History::capture_stroke_rect(&mut capture, doc, min_x, min_y, max_x, max_y)
                },
            );
            let after = doc.layer_tree.find_node(id).unwrap().mask().clone();
            let events =
                History::stroke_recovery_events(&capture, &doc, doc.active_set_id).unwrap();
            assert!(!events.is_empty());
            for event in events {
                event.apply(&mut saved).unwrap();
            }
            assert_eq!(saved.layer_tree.find_node(id).unwrap().mask(), &after);
            assert_eq!(
                history.commit_stroke(capture, &mut doc),
                StrokeCommitResult::Committed
            );
            assert!(history.undo(&mut doc));
            assert_eq!(
                doc.layer_tree
                    .find_node(id)
                    .unwrap()
                    .mask()
                    .get_pixel(32, 32)
                    .0,
                [255]
            );
            assert!(history.redo(&mut doc));
            assert_eq!(doc.layer_tree.find_node(id).unwrap().mask(), &after);
            assert_eq!(
                doc.layer_tree
                    .find_node(base_id)
                    .unwrap()
                    .mask()
                    .get_pixel(32, 32)
                    .0,
                [255]
            );
        }
    }

    #[test]
    fn stroke_recovery_events_replay_channel_bytes_exactly() {
        let mut doc = Document::new(128);
        let mut saved = Document::new(128);
        paint_root_mut(&mut saved, 0).clone_from(paint_root(&doc, 0));
        let mut capture = History::begin_stroke(&doc, Channel::Roughness, BrushMode::Paint);
        assert!(History::capture_stroke_rect(
            &mut capture,
            &doc,
            13,
            27,
            13,
            27
        ));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Roughness, 128)
            .put_pixel(13, 27, Rgba([17, 83, 149, 231]));

        let events = History::stroke_recovery_events(&capture, &doc, doc.active_set_id).unwrap();
        assert_eq!(events.len(), 1);
        for event in events {
            event.apply(&mut saved).unwrap();
        }
        assert_eq!(
            paint_root(&saved, 0).pixels[&Channel::Roughness].as_raw(),
            paint_root(&doc, 0).pixels[&Channel::Roughness].as_raw()
        );
    }

    #[test]
    fn stroke_recovery_events_replay_mask_bytes_exactly() {
        let mut doc = Document::new(128);
        let mut saved = Document::new(128);
        paint_root_mut(&mut saved, 0).clone_from(paint_root(&doc, 0));
        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::MaskWhite);
        assert!(History::capture_stroke_rect(
            &mut capture,
            &doc,
            63,
            64,
            64,
            65
        ));
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(64, 65, image::Luma([37]));

        let events = History::stroke_recovery_events(&capture, &doc, doc.active_set_id).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            DocumentEvent::LayerImagePatch {
                target: LayerImageTarget::Mask,
                ..
            }
        ));
        for event in events {
            event.apply(&mut saved).unwrap();
        }
        assert_eq!(
            paint_root(&saved, 0).mask.as_raw(),
            paint_root(&doc, 0).mask.as_raw()
        );
    }

    #[test]
    fn unchanged_stroke_produces_no_recovery_events() {
        let doc = Document::new(64);
        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(&mut capture, &doc, 2, 3, 4, 5));
        assert!(
            History::stroke_recovery_events(&capture, &doc, doc.active_set_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stroke_recovery_event_carries_clipped_boundary_tile() {
        let mut doc = Document::new(130);
        let mut saved = Document::new(130);
        paint_root_mut(&mut saved, 0).clone_from(paint_root(&doc, 0));
        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(
            &mut capture,
            &doc,
            129,
            129,
            129,
            129
        ));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 130)
            .put_pixel(129, 129, Rgba([4, 3, 2, 1]));

        let events = History::stroke_recovery_events(&capture, &doc, doc.active_set_id).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            DocumentEvent::LayerImagePatch {
                x: 128,
                y: 128,
                width: 2,
                height: 2,
                bytes,
                ..
            } if bytes.len() == 16
        ));
        events[0].apply(&mut saved).unwrap();
        assert_eq!(
            paint_root(&saved, 0).pixels[&Channel::BaseColor].as_raw(),
            paint_root(&doc, 0).pixels[&Channel::BaseColor].as_raw()
        );
    }

    #[test]
    fn snapshot_recovery_events_replay_channel_and_mask_fill_changes() {
        let mut doc = Document::new(130);
        let mut saved = Document::new(130);
        paint_root_mut(&mut saved, 0).clone_from(paint_root(&doc, 0));
        let layer_id = doc.selected_layer().unwrap().id;
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 130)
            .put_pixel(129, 129, image::Rgba([17, 31, 47, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        let channel_snapshot = Snapshot {
            layer_id,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        };
        let channel_events =
            History::snapshot_recovery_events(&channel_snapshot, &doc, doc.active_set_id).unwrap();
        assert_eq!(channel_events.len(), 1);
        assert!(matches!(
            &channel_events[0],
            DocumentEvent::LayerImagePatch {
                x: 128,
                y: 128,
                width: 2,
                height: 2,
                bytes,
                ..
            } if bytes[12..] == [17, 31, 47, 255]
        ));
        for event in channel_events {
            event.apply(&mut saved).unwrap();
        }
        assert_eq!(
            paint_root(&saved, 0).pixels[&Channel::BaseColor],
            paint_root(&doc, 0).pixels[&Channel::BaseColor]
        );

        let before = History::capture(&doc, Channel::BaseColor, BrushMode::MaskWhite);
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(64, 65, image::Luma([73]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::MaskWhite);
        let mask_snapshot = Snapshot {
            layer_id,
            channel: Channel::BaseColor,
            mask: true,
            channel_was_present: true,
            before,
            after,
        };
        let mask_events =
            History::snapshot_recovery_events(&mask_snapshot, &doc, doc.active_set_id).unwrap();
        assert_eq!(mask_events.len(), 1);
        for event in mask_events {
            event.apply(&mut saved).unwrap();
        }
        assert_eq!(paint_root(&saved, 0).mask, paint_root(&doc, 0).mask);
    }

    #[test]
    fn over_limit_stroke_preserves_redo_and_rolls_back_partial_dabs() {
        let mut doc = Document::new(128);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 128)
            .put_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        });
        assert!(history.undo(&mut doc));

        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(&mut capture, &doc, 1, 1, 2, 2));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 128)
            .put_pixel(2, 2, Rgba([90, 80, 70, 255]));
        capture.reserved_bytes = MAX_HISTORY_BYTES;
        assert!(!History::capture_stroke_rect(
            &mut capture,
            &doc,
            64,
            64,
            65,
            65
        ));
        assert_eq!(
            history.commit_stroke(capture, &mut doc),
            StrokeCommitResult::OverLimit
        );
        assert_eq!(history.undo.len(), 0);
        assert_eq!(history.redo.len(), 1);
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
                .get_pixel(1, 1)
                .0,
            [10, 20, 30, 255]
        );
    }

    #[test]
    fn stroke_history_survives_layer_properties_and_stack_additions() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, Rgba([90, 80, 70, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        });

        let layer = doc.selected_layer_mut().unwrap();
        layer.name = "Renamed".into();
        layer.visible = false;
        layer.opacity = 0.4;
        layer.blend = crate::document::BlendMode::Multiply;
        doc.add_layer().unwrap();

        assert!(history.undo(&mut doc));
        assert!(!paint_root(&doc, 0).pixels.contains_key(&Channel::BaseColor));
        assert_eq!(paint_root(&doc, 0).name, "Renamed");
        assert_eq!(paint_root(&doc, 0).opacity, 0.4);
        assert_eq!(doc.layer_tree.roots.len(), 2);
        assert!(history.redo(&mut doc));
        assert_eq!(
            paint_root(&doc, 0).pixels[&Channel::BaseColor]
                .get_pixel(3, 4)
                .0,
            [90, 80, 70, 255]
        );
    }

    #[test]
    fn remove_undo_restores_layer_then_its_prior_stroke() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(5, 6, Rgba([12, 34, 56, 255]));
        let after = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        });

        let selected_before = paint_root(&doc, selected_root_index(&doc)).id;
        doc.add_layer().unwrap();
        let added_id = paint_root(&doc, selected_root_index(&doc)).id;
        history.record_added_layer(&doc, added_id, selected_before, None);
        let props_before = LayerProperties::capture(doc.selected_layer().unwrap());
        {
            let layer = doc.selected_layer_mut().unwrap();
            layer.name = "Marked layer".into();
            layer.visible = false;
            layer.opacity = 0.35;
            layer.mask.put_pixel(2, 3, image::Luma([77]));
            layer
                .pixels_mut(Channel::Metallic, 64)
                .put_pixel(2, 3, Rgba([99, 99, 99, 255]));
        }
        history.record_properties(
            added_id,
            props_before,
            LayerProperties::capture(doc.selected_layer().unwrap()),
        );

        let selected_before_remove = paint_root(&doc, selected_root_index(&doc)).id;
        let remove_index = selected_root_index(&doc);
        let removed = doc.remove_layer().unwrap();
        let selected_after_remove = paint_root(&doc, selected_root_index(&doc)).id;
        history.record_removed_layer(
            removed,
            remove_index,
            selected_before_remove,
            selected_after_remove,
        );

        assert!(history.undo(&mut doc));
        assert_eq!(paint_root(&doc, selected_root_index(&doc)).id, added_id);
        assert_eq!(doc.selected_layer().unwrap().name, "Marked layer");
        assert_eq!(doc.selected_layer().unwrap().mask.get_pixel(2, 3).0, [77]);
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::Metallic]
                .get_pixel(2, 3)
                .0,
            [99, 99, 99, 255]
        );
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().unwrap().name, "Paint Layer 2");
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(paint_root(&doc, 0).id, 1);
        assert!(history.undo(&mut doc));
        assert!(!paint_root(&doc, 0).pixels.contains_key(&Channel::BaseColor));

        for _ in 0..4 {
            assert!(history.redo(&mut doc));
        }
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(paint_root(&doc, 0).id, 1);
        assert!(history.used_bytes <= MAX_HISTORY_BYTES);
    }

    #[test]
    fn property_edits_coalesce_and_restore_complete_state() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = LayerProperties::capture(doc.selected_layer().unwrap());
        doc.selected_layer_mut().unwrap().opacity = 0.8;
        doc.selected_layer_mut().unwrap().name = "First edit".into();
        let intermediate = LayerProperties::capture(doc.selected_layer().unwrap());
        history.record_properties(1, before.clone(), intermediate.clone());
        doc.selected_layer_mut().unwrap().opacity = 0.25;
        doc.selected_layer_mut().unwrap().visible = false;
        doc.selected_layer_mut()
            .unwrap()
            .disabled_channels
            .insert(Channel::Normal);
        doc.selected_layer_mut().unwrap().blend = BlendMode::Add;
        doc.selected_layer_mut().unwrap().name = "Final edit".into();
        doc.selected_layer_mut().unwrap().procedural_graph =
            Some(ProceduralGraph::value_noise_times_occlusion(91, 6.0, 37));
        history.record_properties(
            1,
            intermediate,
            LayerProperties::capture(doc.selected_layer().unwrap()),
        );

        assert_eq!(history.undo.len(), 1);
        let recovery = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(recovery.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerTreeNodePropertiesSet {
                procedural_graph: None,
                ..
            }
        )));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().unwrap().name, before.name);
        assert_eq!(doc.selected_layer().unwrap().opacity, before.opacity);
        assert_eq!(doc.selected_layer().unwrap().visible, before.visible);
        assert_eq!(doc.selected_layer().unwrap().blend, before.blend);
        assert!(doc.selected_layer().unwrap().disabled_channels.is_empty());
        assert!(doc.selected_layer().unwrap().procedural_graph.is_none());
        assert!(history.redo(&mut doc));
        assert_eq!(doc.selected_layer().unwrap().name, "Final edit");
        assert_eq!(doc.selected_layer().unwrap().opacity, 0.25);
        assert!(!doc.selected_layer().unwrap().visible);
        assert_eq!(doc.selected_layer().unwrap().blend, BlendMode::Add);
        assert!(doc
            .selected_layer()
            .unwrap()
            .disabled_channels
            .contains(&Channel::Normal));
        assert_eq!(
            doc.selected_layer().unwrap().procedural_graph,
            Some(ProceduralGraph::value_noise_times_occlusion(91, 6.0, 37))
        );
    }

    #[test]
    fn duplicate_and_reorder_restore_payload_order_and_selection() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 8, Rgba([4, 5, 6, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(7, 8, image::Luma([123]));

        let base_id = paint_root(&doc, selected_root_index(&doc)).id;
        doc.duplicate_selected().unwrap();
        let duplicate_id = paint_root(&doc, selected_root_index(&doc)).id;
        history.record_added_layer(&doc, duplicate_id, base_id, Some(base_id));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(paint_root(&doc, selected_root_index(&doc)).id, base_id);
        assert!(history.redo(&mut doc));
        assert_eq!(paint_root(&doc, selected_root_index(&doc)).id, duplicate_id);
        assert_eq!(paint_root(&doc, 1).name, "Paint Layer copy");
        assert_eq!(paint_root(&doc, 1).mask.get_pixel(7, 8).0, [123]);
        assert_eq!(
            paint_root(&doc, 1).pixels[&Channel::BaseColor]
                .get_pixel(7, 8)
                .0,
            [4, 5, 6, 255]
        );

        doc.layer_tree.select(doc.layer_tree.roots[0].id()).unwrap();
        let before_source_edit = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 8, Rgba([200, 201, 202, 255]));
        let after_source_edit = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: base_id,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: true,
            before: before_source_edit,
            after: after_source_edit,
        });
        assert!(history.undo(&mut doc));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert!(history.redo(&mut doc));
        assert_eq!(paint_root(&doc, 1).id, duplicate_id);
        assert_eq!(
            paint_root(&doc, 1).pixels[&Channel::BaseColor]
                .get_pixel(7, 8)
                .0,
            [4, 5, 6, 255]
        );

        doc.add_layer().unwrap();
        let third_id = paint_root(&doc, selected_root_index(&doc)).id;
        history.record_added_layer(&doc, third_id, duplicate_id, None);
        let old_order: Vec<_> = doc.layer_tree.roots.iter().map(LayerNode::id).collect();
        doc.layer_tree.select(doc.layer_tree.roots[1].id()).unwrap();
        let old_index = selected_root_index(&doc);
        let moved_id = paint_root(&doc, old_index).id;
        let selected_before = moved_id;
        doc.move_selected(true);
        let new_index = selected_root_index(&doc);
        let selected_after = paint_root(&doc, selected_root_index(&doc)).id;
        history.record_moved_layer(
            moved_id,
            old_index,
            new_index,
            selected_before,
            selected_after,
        );
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(LayerNode::id)
                .collect::<Vec<_>>(),
            old_order
        );
        assert_eq!(paint_root(&doc, selected_root_index(&doc)).id, moved_id);
        assert!(history.redo(&mut doc));
        assert_eq!(paint_root(&doc, selected_root_index(&doc)).id, moved_id);
        assert_eq!(paint_root(&doc, 2).id, moved_id);
    }

    #[test]
    fn removed_layer_payload_limit_has_an_exact_boundary() {
        assert!(history_payload_within_limit(MAX_HISTORY_BYTES));
        assert!(!history_payload_within_limit(MAX_HISTORY_BYTES + 1));
    }

    #[test]
    fn full_snapshot_upper_bound_respects_history_budget() {
        assert!(!History::snapshot_upper_bound_fits(0, 4));
        assert!(!History::snapshot_upper_bound_fits(4096, 4));
        assert!(History::snapshot_upper_bound_fits(2048, 4));
        assert!(History::snapshot_upper_bound_fits(4096, 1));
        assert!(!History::snapshot_upper_bound_fits(u32::MAX, 4));
    }

    #[test]
    fn history_entry_cap_evicts_oldest_entries_first() {
        let mut history = History::new();
        for layer_id in 1..=(MAX_HISTORY as u32 + 1) {
            history.push(Entry::Stack(StackDelta {
                change: StackChange::Added {
                    name: format!("Layer {layer_id}"),
                    duplicate_source: None,
                    payload: None,
                },
                layer_id,
                selected_before: 0,
                selected_after: layer_id,
                index: layer_id as usize,
            }));
        }

        assert_eq!(history.undo.len(), MAX_HISTORY);
        assert!(history.redo.is_empty());
        assert!(matches!(
            history.undo.first(),
            Some(Entry::Stack(StackDelta { layer_id: 2, .. }))
        ));
        assert!(matches!(
            history.undo.last(),
            Some(Entry::Stack(StackDelta {
                layer_id,
                ..
            })) if *layer_id == MAX_HISTORY as u32 + 1
        ));
    }

    #[test]
    fn history_byte_cap_evicts_oldest_entries_first() {
        let mut history = History::new();
        let entry_bytes = MAX_HISTORY_BYTES * 2 / 3;
        for layer_id in 1..=2 {
            history.push(Entry::Pixels(Delta {
                layer_id,
                channel: Channel::BaseColor,
                mask: false,
                channel_was_present: true,
                patches: Vec::new(),
                size: entry_bytes,
            }));
        }

        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.used_bytes, entry_bytes);
        assert!(matches!(
            history.undo.first(),
            Some(Entry::Pixels(Delta { layer_id: 2, .. }))
        ));
    }

    #[test]
    fn oversized_layer_delete_is_rejected_without_document_or_history_changes() {
        let mut doc = Document::new(64);
        let mut large = Layer::new(2, "Large", 64);
        large.extensions.insert(
            "futurePayload".to_owned(),
            serde_json::Value::String("x".repeat(MAX_HISTORY_BYTES + 1)),
        );
        large.mask.put_pixel(4, 5, image::Luma([67]));
        doc.layer_tree.next_node_id = 3;
        // Deliberately bypass the public insertion preflight to model an
        // invalid in-memory tree (for example, one built by a future caller).
        doc.layer_tree.roots.push(LayerNode::Paint(large));
        doc.layer_tree.select(doc.layer_tree.roots[1].id()).unwrap();
        let mut history = History::new();
        let original_ids: Vec<_> = doc.layer_tree.roots.iter().map(LayerNode::id).collect();
        let original_name = doc.selected_layer().unwrap().name.clone();
        let original_mask_pixel = doc.selected_layer().unwrap().mask.get_pixel(4, 5).0;

        assert!(history.remove_selected_layer(&mut doc).is_err());
        assert_eq!(doc.layer_tree.roots.len(), 2);
        assert_eq!(selected_root_index(&doc), 1);
        assert_eq!(doc.layer_tree.next_node_id, 3);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(LayerNode::id)
                .collect::<Vec<_>>(),
            original_ids
        );
        assert_eq!(doc.selected_layer().unwrap().name, original_name);
        assert_eq!(
            doc.selected_layer().unwrap().mask.get_pixel(4, 5).0,
            original_mask_pixel
        );
        assert!(history.undo.is_empty());
        assert!(history.redo.is_empty());
        assert_eq!(history.used_bytes, 0);
    }

    #[test]
    fn no_op_does_not_create_history_entry() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&doc, Channel::BaseColor, BrushMode::Paint);
        let after = before.clone();
        assert!(!history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: false,
            before,
            after,
        }));
        assert!(!history.undo(&mut doc));
    }

    #[test]
    fn oversized_snapshot_preserves_undo_and_invalidates_redo() {
        let mut history = History::new();
        history.undo.push(Entry::Pixels(Delta {
            layer_id: 7,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: true,
            patches: Vec::new(),
            size: 23,
        }));
        history.redo.push(Entry::Pixels(Delta {
            layer_id: 8,
            channel: Channel::Roughness,
            mask: false,
            channel_was_present: true,
            patches: Vec::new(),
            size: 17,
        }));
        history.recompute_bytes();
        assert_eq!(history.used_bytes, 40);

        // Inject the computed size so this exercises the oversized commit
        // path without allocating a >96 MiB before/after image pair.
        assert!(!history.commit_snapshot_with_storage(
            Snapshot {
                layer_id: 1,
                channel: Channel::BaseColor,
                mask: false,
                channel_was_present: true,
                before: vec![0],
                after: vec![1],
            },
            MAX_HISTORY_BYTES + 1,
        ));

        assert_eq!(history.undo.len(), 1);
        assert!(matches!(
            history.undo.last(),
            Some(Entry::Pixels(Delta { layer_id: 7, .. }))
        ));
        assert!(history.redo.is_empty());
        assert_eq!(history.used_bytes, 23);
        assert!(history.used_bytes <= MAX_HISTORY_BYTES);
    }

    #[test]
    fn recovery_plans_choose_pixel_and_mask_bytes_for_each_direction() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
        let before = [10, 20, 30, 40];
        let after = [50, 60, 70, 80];
        let image = doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64);
        image.as_mut()[..4].copy_from_slice(&after);
        let mut history = History::new();
        history.push(Entry::Pixels(Delta {
            layer_id,
            channel: Channel::BaseColor,
            mask: false,
            channel_was_present: true,
            patches: vec![Patch {
                offset: 0,
                row_bytes: 4,
                rows: 1,
                stride: 256,
                before: before.to_vec(),
                after: after.to_vec(),
            }],
            size: 8,
        }));

        let undo = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            &undo[0],
            DocumentEvent::LayerImagePatch {
                target: LayerImageTarget::Channel(Channel::BaseColor),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                bytes,
                ..
            } if bytes == &before
        ));
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor].as_raw()[..4],
            after
        );
        assert_eq!(history.undo.len(), 1, "planning must not mutate history");

        doc.selected_layer_mut().unwrap().mask.as_mut()[0] = 9;
        history.redo.clear();
        history.push(Entry::Pixels(Delta {
            layer_id,
            channel: Channel::Roughness,
            mask: true,
            channel_was_present: true,
            patches: vec![Patch {
                offset: 0,
                row_bytes: 1,
                rows: 1,
                stride: 64,
                before: vec![3],
                after: vec![9],
            }],
            size: 2,
        }));
        let undo_mask = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            &undo_mask[0],
            DocumentEvent::LayerImagePatch {
                target: LayerImageTarget::Mask,
                bytes,
                ..
            } if bytes == &[3]
        ));
    }

    #[test]
    fn recovery_plan_removes_absent_channel_on_undo_and_recreates_it_on_redo() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
        let channel = Channel::Metallic;
        let after = [77, 88, 99, 255];
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(channel, 64)
            .as_mut()[..4]
            .copy_from_slice(&after);
        let entry = Entry::Pixels(Delta {
            layer_id,
            channel,
            mask: false,
            channel_was_present: false,
            patches: vec![Patch {
                offset: 0,
                row_bytes: 4,
                rows: 1,
                stride: 256,
                before: vec![0; 4],
                after: after.to_vec(),
            }],
            size: 8,
        });
        let mut history = History::new();
        history.push(entry);
        let undo = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            undo.first(),
            Some(DocumentEvent::LayerChannelRemove {
                channel: Channel::Metallic,
                layer_id: 1,
                ..
            })
        ));

        assert!(history.undo(&mut doc));
        assert!(!doc.selected_layer().unwrap().pixels.contains_key(&channel));
        let redo = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Redo)
            .unwrap();
        assert!(matches!(
            &redo[0],
            DocumentEvent::LayerImagePatch {
                target: LayerImageTarget::Channel(Channel::Metallic),
                bytes,
                ..
            } if bytes == &after
        ));
    }

    #[test]
    fn recovery_plan_splits_flat_snapshot_patches_at_image_rows() {
        let size = 65;
        let mut doc = Document::new(size);
        let layer_id = doc.selected_layer().unwrap().id;
        let length = size as usize * size as usize * 4;
        let before = vec![0; length];
        let mut after = before.clone();
        for index in [4092usize, 4096, 4104, 8192] {
            after[index..index + 4].copy_from_slice(&[11, 22, 33, 44]);
        }
        let image = doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Height, size);
        image.as_mut().copy_from_slice(&after);
        let mut history = History::new();
        history.commit(Snapshot {
            layer_id,
            channel: Channel::Height,
            mask: false,
            channel_was_present: true,
            before: before.clone(),
            after,
        });

        let events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        let mut reconstructed = doc.selected_layer().unwrap().pixels[&Channel::Height]
            .as_raw()
            .clone();
        for event in &events {
            if let DocumentEvent::LayerImagePatch {
                x,
                y,
                width,
                height,
                bytes,
                ..
            } = event
            {
                assert_eq!(*height, 1);
                assert!(*x + *width <= size);
                assert!(*y < size);
                let start = (*y as usize * size as usize + *x as usize) * 4;
                reconstructed[start..start + bytes.len()].copy_from_slice(bytes);
            }
        }
        assert_eq!(reconstructed, before);
    }

    #[test]
    fn recovery_plans_properties_moves_and_simple_stack_operations() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let layer_id = doc.selected_layer().unwrap().id;
        let before = LayerProperties::capture(doc.selected_layer().unwrap());
        doc.selected_layer_mut().unwrap().visible = false;
        doc.selected_layer_mut().unwrap().opacity = 0.5;
        let after = LayerProperties::capture(doc.selected_layer().unwrap());
        history.record_properties(layer_id, before, after);
        let property_events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            property_events.as_slice(),
            [DocumentEvent::LayerTreeNodePropertiesSet {
                set_id: 1,
                node_id: 1,
                name,
                visible: true,
                opacity,
                ..
            }] if name == "Paint Layer" && *opacity == 1.0
        ));

        let mut moved = Document::new(64);
        moved.add_layer().unwrap();
        moved.add_layer().unwrap();
        // Current order is [1, 2, 3]. Record moving ID 2 from 1 to 0,
        // retaining selected layer 3.
        moved.layer_tree.roots.swap(0, 1);
        let mut move_history = History::new();
        move_history.record_moved_layer(2, 1, 0, 3, 3);
        let move_events = move_history
            .prepare_recovery_events(&moved, moved.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            move_events.first(),
            Some(DocumentEvent::LayerReorder {
                layer_id: 2,
                to_index: 1,
                ..
            })
        ));
        assert!(matches!(
            move_events.last(),
            Some(DocumentEvent::LayerSelection { layer_id: 3, .. })
        ));

        let mut added = Document::new(64);
        let selected_before = added.selected_layer().unwrap().id;
        added.add_layer().unwrap();
        let added_id = added.selected_layer().unwrap().id;
        let mut add_history = History::new();
        add_history.record_added_layer(&added, added_id, selected_before, None);
        let undo_add = add_history
            .prepare_recovery_events(&added, added.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            undo_add.first(),
            Some(DocumentEvent::LayerSelection { layer_id, .. }) if *layer_id == added_id
        ));
        assert!(matches!(
            undo_add.get(1),
            Some(DocumentEvent::LayerRemove { layer_id, selected_after_id: 1, .. }) if *layer_id == added_id
        ));
        assert!(add_history.undo(&mut added));
        let redo_add = add_history
            .prepare_recovery_events(&added, added.active_set_id, HistoryDirection::Redo)
            .unwrap();
        assert!(matches!(
            redo_add.first(),
            Some(DocumentEvent::LayerReinsert {
                layer_id,
                source_layer_id: None,
                ..
            }) if *layer_id == added_id
        ));
        assert!(matches!(
            redo_add.last(),
            Some(DocumentEvent::LayerSelection { layer_id, .. }) if *layer_id == added_id
        ));

        let mut duplicate = Document::new(64);
        let selected_before = duplicate.selected_layer().unwrap().id;
        duplicate.add_layer().unwrap();
        let duplicate_id = duplicate.selected_layer().unwrap().id;
        let mut duplicate_history = History::new();
        duplicate_history.record_added_layer(
            &duplicate,
            duplicate_id,
            selected_before,
            Some(selected_before),
        );
        assert!(duplicate_history.undo(&mut duplicate));
        let redo_duplicate = duplicate_history
            .prepare_recovery_events(&duplicate, duplicate.active_set_id, HistoryDirection::Redo)
            .unwrap();
        assert!(matches!(
            redo_duplicate.first(),
            Some(DocumentEvent::LayerReinsert {
                layer_id,
                source_layer_id: Some(1),
                ..
            }) if *layer_id == duplicate_id
        ));
    }

    #[test]
    fn group_mask_invert_undo_redo_has_recovery_event_for_stable_node_id() {
        let mut doc = Document::new(64);
        let group_id = 90;
        let mut properties = LayerNodeProperties::new(group_id, "Mask group", doc.size).unwrap();
        properties.mask.put_pixel(2, 3, image::Luma([42]));
        doc.layer_tree.roots.push(LayerNode::Group(LayerGroup {
            properties,
            children: Vec::new(),
        }));
        doc.layer_tree.next_node_id = group_id + 1;
        doc.select_node(group_id).unwrap();

        // Match the editor action: the change is applied first, then retained
        // as a small invert operation in history.
        let group = doc.layer_tree.find_node_mut(group_id).unwrap();
        let LayerNode::Group(group) = group else {
            unreachable!();
        };
        for pixel in group.properties.mask.as_mut() {
            *pixel = 255 - *pixel;
        }
        assert_eq!(group.properties.mask.get_pixel(2, 3).0, [213]);

        let mut history = History::new();
        history.record_mask_invert(group_id);
        let undo_events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(undo_events.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerMaskInvert { layer_id, .. } if *layer_id == group_id
        )));

        assert!(history.undo(&mut doc));
        let LayerNode::Group(group) = doc.layer_tree.find_node(group_id).unwrap() else {
            unreachable!();
        };
        assert_eq!(group.properties.mask.get_pixel(2, 3).0, [42]);

        let redo_events = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Redo)
            .unwrap();
        assert!(redo_events.iter().any(|event| matches!(
            event,
            DocumentEvent::LayerMaskInvert { layer_id, .. } if *layer_id == group_id
        )));

        assert!(history.redo(&mut doc));
        let LayerNode::Group(group) = doc.layer_tree.find_node(group_id).unwrap() else {
            unreachable!();
        };
        assert_eq!(group.properties.mask.get_pixel(2, 3).0, [213]);
    }

    #[test]
    fn recovery_planner_snapshots_retained_stack_payloads() {
        let mut doc = Document::new(64);
        let selected_before = doc.selected_layer().unwrap().id;
        doc.add_layer().unwrap();
        let imported_id = doc.selected_layer().unwrap().id;
        let mut history = History::new();
        assert!(history.record_added_layer_with_payload(&doc, imported_id, selected_before));
        assert!(history.undo(&mut doc));
        let redo = history
            .prepare_recovery_events(&doc, doc.active_set_id, HistoryDirection::Redo)
            .unwrap();
        let mut visited = Vec::new();
        history
            .visit_recovery_events(&doc, doc.active_set_id, HistoryDirection::Redo, |event| {
                visited.push(event);
                true
            })
            .unwrap();
        assert_eq!(visited, redo);
        assert!(matches!(
            redo.first(),
            Some(DocumentEvent::LayerSnapshotBegin { layer_id, .. }) if *layer_id == imported_id
        ));
        assert!(matches!(
            redo.get(1),
            Some(DocumentEvent::LayerSnapshotChunk {
                target: LayerImageTarget::Mask,
                offset: 0,
                ..
            })
        ));
        assert!(matches!(
            redo.last(),
            Some(DocumentEvent::LayerSelection { layer_id, .. }) if *layer_id == imported_id
        ));

        let mut accepted = 0;
        assert!(history
            .visit_recovery_events(&doc, doc.active_set_id, HistoryDirection::Redo, |_| {
                accepted += 1;
                accepted < 3
            })
            .is_none());
        assert_eq!(accepted, 3, "visitor stops at the first rejected event");

        let mut removed = Document::new(64);
        removed.add_layer().unwrap();
        let mut removed_history = History::new();
        assert!(removed_history.remove_selected_layer(&mut removed).unwrap());
        let undo = removed_history
            .prepare_recovery_events(&removed, removed.active_set_id, HistoryDirection::Undo)
            .unwrap();
        assert!(matches!(
            undo.first(),
            Some(DocumentEvent::LayerSnapshotBegin { layer_id: 2, .. })
        ));
    }
}

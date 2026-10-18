//! Bounded, sparse undo/redo history. Interactive strokes retain only changed
//! rows from touched 64×64 tiles; fill/clear snapshots are split into chunks.
use crate::document::{BlendMode, Channel, Document, Layer};
use crate::paint::BrushMode;
use crate::recovery::{DocumentEvent, LayerImageTarget};
use std::collections::{HashMap, HashSet};

pub const MAX_HISTORY: usize = 64;
pub const MAX_HISTORY_BYTES: usize = 96 * 1024 * 1024;
const CHUNK_BYTES: usize = 4096;
pub const STROKE_TILE_SIZE: u32 = 64;
const MAX_RECOVERY_EVENT_DATA_BYTES: usize = 1024 * 1024;

pub struct Snapshot {
    pub layer_id: u32,
    pub channel: Channel,
    pub mask: bool,
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
    let layer = doc
        .layers
        .iter()
        .find(|layer| layer.id == capture.layer_id)?;
    let source: Option<&[u8]> = if capture.mask {
        Some(layer.mask.as_raw())
    } else {
        layer
            .pixels
            .get(&capture.channel)
            .map(|image| image.as_raw().as_slice())
    };
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
        let layer = doc
            .layers
            .iter()
            .find(|layer| layer.id == snapshot.layer_id)?;
        let current: Option<&[u8]> = if snapshot.mask {
            Some(layer.mask.as_raw())
        } else {
            layer
                .pixels
                .get(&snapshot.channel)
                .map(|image| image.as_raw().as_slice())
        };
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
}

fn restore_stroke_before_images(capture: &StrokeCapture, doc: &mut Document) {
    let size = doc.size;
    let Some(layer) = doc
        .layers
        .iter_mut()
        .find(|layer| layer.id == capture.layer_id)
    else {
        return;
    };
    if !capture.mask && !capture.channel_was_present {
        layer.pixels.remove(&capture.channel);
        return;
    }
    let bytes_per_pixel = if capture.mask { 1 } else { 4 };
    let stride = size as usize * bytes_per_pixel;
    let target: &mut [u8] = if capture.mask {
        layer.mask.as_mut()
    } else {
        layer.pixels_mut(capture.channel, size).as_mut()
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
}

impl LayerProperties {
    pub fn capture(layer: &Layer) -> Self {
        Self {
            name: layer.name.clone(),
            visible: layer.visible,
            opacity: layer.opacity,
            blend: layer.blend,
            disabled_channels: layer.disabled_channels.clone(),
        }
    }

    fn storage_bytes(&self) -> usize {
        self.name
            .len()
            .saturating_add(self.disabled_channels.len().saturating_mul(8))
            .saturating_add(std::mem::size_of::<bool>() * 2)
            .saturating_add(std::mem::size_of::<f32>())
            .saturating_add(std::mem::size_of::<BlendMode>())
    }

    fn apply(&self, layer: &mut Layer) {
        layer.name.clone_from(&self.name);
        layer.visible = self.visible;
        layer.opacity = self.opacity;
        layer.blend = self.blend;
        layer.disabled_channels.clone_from(&self.disabled_channels);
    }
}

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

struct PropertyDelta {
    layer_id: u32,
    before: LayerProperties,
    after: LayerProperties,
}

enum Entry {
    Pixels(Delta),
    Stack(StackDelta),
    Properties(PropertyDelta),
}

impl Entry {
    fn storage_bytes(&self) -> usize {
        match self {
            Self::Pixels(delta) => delta.size,
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
            Self::Properties(delta) => delta
                .before
                .storage_bytes()
                .saturating_add(delta.after.storage_bytes()),
        }
    }
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
    let Some(index) = doc.layers.iter().position(|layer| layer.id == layer_id) else {
        return false;
    };
    doc.selected = index;
    true
}

pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    used_bytes: usize,
}

fn history_payload_within_limit(bytes: usize) -> bool {
    bytes <= MAX_HISTORY_BYTES
}

impl History {
    pub fn new() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            used_bytes: 0,
        }
    }
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.used_bytes = 0;
    }
    pub fn capture(doc: &mut Document, channel: Channel, mode: BrushMode) -> Vec<u8> {
        let size = doc.size;
        let layer = doc.selected_layer_mut();
        if mode.is_mask() {
            layer.mask.as_raw().clone()
        } else {
            layer.pixels_mut(channel, size).as_raw().clone()
        }
    }

    pub fn begin_stroke(doc: &Document, channel: Channel, mode: BrushMode) -> StrokeCapture {
        StrokeCapture {
            layer_id: doc.selected_layer().id,
            channel,
            mask: mode.is_mask(),
            channel_was_present: mode.is_mask()
                || doc.selected_layer().pixels.contains_key(&channel),
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
        let Some(layer) = doc.layers.iter().find(|layer| layer.id == capture.layer_id) else {
            capture.over_limit = true;
            capture.tiles.clear();
            capture.reserved_bytes = 0;
            return false;
        };
        let source: Option<&[u8]> = if capture.mask {
            Some(layer.mask.as_raw())
        } else {
            layer
                .pixels
                .get(&capture.channel)
                .map(|image| image.as_raw().as_slice())
        };
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
    pub fn commit(&mut self, entry: Snapshot) {
        if entry.before.len() != entry.after.len() || entry.before == entry.after {
            return;
        }
        let mut patches = Vec::new();
        let mut stored = 0usize;
        for (index, (before, after)) in entry
            .before
            .chunks(CHUNK_BYTES)
            .zip(entry.after.chunks(CHUNK_BYTES))
            .enumerate()
        {
            if before == after {
                continue;
            }
            stored = stored
                .saturating_add(before.len().saturating_mul(2))
                .saturating_add(std::mem::size_of::<Patch>());
            patches.push(Patch {
                offset: index * CHUNK_BYTES,
                row_bytes: before.len(),
                rows: 1,
                stride: before.len(),
                before: before.to_vec(),
                after: after.to_vec(),
            });
        }
        let delta = Delta {
            layer_id: entry.layer_id,
            channel: entry.channel,
            mask: entry.mask,
            channel_was_present: true,
            patches,
            size: stored,
        };
        // A massive full-channel replacement may exceed the history budget.
        // Apply the edit but do not claim it is undoable; retain older undo.
        if stored > MAX_HISTORY_BYTES {
            self.clear();
            return;
        }
        self.push(Entry::Pixels(delta));
    }

    pub fn can_retain_removed_layer(layer: &Layer) -> bool {
        history_payload_within_limit(layer_storage_bytes(layer))
    }

    pub fn record_added_layer(
        &mut self,
        doc: &Document,
        layer_id: u32,
        selected_before: u32,
        duplicate_source: Option<u32>,
    ) {
        let Some(index) = doc.layers.iter().position(|layer| layer.id == layer_id) else {
            return;
        };
        let selected_after = doc.layers[doc.selected].id;
        let name = doc.layers[index].name.clone();
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
        let Some(index) = doc.layers.iter().position(|layer| layer.id == layer_id) else {
            return false;
        };
        let layer = &doc.layers[index];
        if !history_payload_within_limit(layer_storage_bytes(layer)) {
            return false;
        }
        let selected_after = doc.layers[doc.selected].id;
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

    pub fn remove_selected_layer(
        &mut self,
        doc: &mut Document,
    ) -> std::result::Result<bool, &'static str> {
        if doc.layers.len() <= 1 {
            return Ok(false);
        }
        let layer = &doc.layers[doc.selected];
        if !Self::can_retain_removed_layer(layer) {
            return Err("Layer exceeds the 96-MiB undo payload limit");
        }
        let selected_before = layer.id;
        let index = doc.selected;
        let removed = doc
            .remove_layer()
            .expect("layer count was checked before removal");
        let selected_after = doc.layers[doc.selected].id;
        self.record_removed_layer(removed, index, selected_before, selected_after);
        Ok(true)
    }

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
        let Some(layer) = doc.layers.iter_mut().find(|l| l.id == delta.layer_id) else {
            return false;
        };
        let size = doc.size;
        let target: &mut [u8] = if delta.mask {
            layer.mask.as_mut()
        } else {
            if undo && !delta.channel_was_present {
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
            layer.pixels_mut(delta.channel, size).as_mut()
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

    fn apply_stack(doc: &mut Document, delta: &mut StackDelta, undo: bool) -> bool {
        match &mut delta.change {
            StackChange::Added {
                name,
                duplicate_source,
                payload,
            } => {
                if undo {
                    let Some(position) = doc
                        .layers
                        .iter()
                        .position(|layer| layer.id == delta.layer_id)
                    else {
                        return false;
                    };
                    if doc.layers.len() <= 1
                        || !doc
                            .layers
                            .iter()
                            .any(|layer| layer.id == delta.selected_before)
                    {
                        return false;
                    }
                    let removed = doc.layers.remove(position);
                    if payload.is_some() {
                        *payload = Some(removed);
                    }
                    set_selection(doc, delta.selected_before)
                } else {
                    if doc.layers.len() >= crate::document::MAX_LAYERS
                        || doc
                            .layers
                            .iter()
                            .any(|current| current.id == delta.layer_id)
                        || (delta.selected_after != delta.layer_id
                            && !doc
                                .layers
                                .iter()
                                .any(|current| current.id == delta.selected_after))
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
                        let Some(source) = doc.layers.iter().find(|layer| layer.id == *source_id)
                        else {
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
                    let index = delta.index.min(doc.layers.len());
                    doc.layers.insert(index, layer);
                    set_selection(doc, delta.selected_after)
                }
            }
            StackChange::Removed { payload } => {
                if undo {
                    if payload.is_none() {
                        return false;
                    }
                    if doc.layers.len() >= crate::document::MAX_LAYERS
                        || doc
                            .layers
                            .iter()
                            .any(|current| current.id == delta.layer_id)
                        || (delta.selected_before != delta.layer_id
                            && !doc
                                .layers
                                .iter()
                                .any(|current| current.id == delta.selected_before))
                    {
                        return false;
                    }
                    let layer = payload.take().expect("payload checked above");
                    let index = delta.index.min(doc.layers.len());
                    doc.layers.insert(index, layer);
                    set_selection(doc, delta.selected_before)
                } else {
                    let Some(position) = doc
                        .layers
                        .iter()
                        .position(|layer| layer.id == delta.layer_id)
                    else {
                        return false;
                    };
                    if doc.layers.len() <= 1 || payload.is_some() {
                        return false;
                    }
                    if !doc
                        .layers
                        .iter()
                        .any(|layer| layer.id == delta.selected_after)
                    {
                        return false;
                    }
                    *payload = Some(doc.layers.remove(position));
                    set_selection(doc, delta.selected_after)
                }
            }
            StackChange::Moved {
                old_index,
                new_index,
            } => {
                let Some(position) = doc
                    .layers
                    .iter()
                    .position(|layer| layer.id == delta.layer_id)
                else {
                    return false;
                };
                let target = if undo { *old_index } else { *new_index };
                let selected_id = if undo {
                    delta.selected_before
                } else {
                    delta.selected_after
                };
                if target >= doc.layers.len()
                    || !doc.layers.iter().any(|layer| layer.id == selected_id)
                {
                    return false;
                }
                let layer = doc.layers.remove(position);
                doc.layers.insert(target, layer);
                set_selection(doc, selected_id)
            }
        }
    }

    fn apply(doc: &mut Document, entry: &mut Entry, undo: bool) -> bool {
        match entry {
            Entry::Pixels(delta) => Self::apply_pixels(doc, delta, undo),
            Entry::Stack(delta) => Self::apply_stack(doc, delta, undo),
            Entry::Properties(delta) => {
                let Some(layer) = doc
                    .layers
                    .iter_mut()
                    .find(|layer| layer.id == delta.layer_id)
                else {
                    return false;
                };
                if undo {
                    delta.before.apply(layer);
                } else {
                    delta.after.apply(layer);
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
            self.clear();
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
            self.clear();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    #[test]
    fn sparse_undo_redo_reproduces_pixels() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(1, 1, Rgba([20, 40, 60, 255]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            before,
            after,
        });
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor]
                .get_pixel(1, 1)
                .0,
            [0, 0, 0, 0]
        );
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor]
                .get_pixel(1, 1)
                .0,
            [20, 40, 60, 255]
        );
        assert!(history.used_bytes <= 8192 + std::mem::size_of::<Patch>());
    }

    #[test]
    fn four_k_stroke_capture_is_sparse_and_repeated_tiles_are_captured_once() {
        let mut doc = Document::new(4096);
        let mut history = History::new();
        assert!(!doc
            .selected_layer()
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
            doc.selected_layer().pixels[&Channel::BaseColor]
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
            .pixels
            .contains_key(&Channel::BaseColor));
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor]
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
            .mask
            .put_pixel(7, 9, image::Luma([42]));
        assert_eq!(
            history.commit_stroke(capture, &mut doc),
            StrokeCommitResult::Committed
        );
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().mask.get_pixel(7, 9).0, [255]);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.selected_layer().mask.get_pixel(7, 9).0, [42]);
    }

    #[test]
    fn stroke_recovery_events_replay_channel_bytes_exactly() {
        let mut doc = Document::new(128);
        let mut saved = Document::new(128);
        saved.layers[0] = doc.layers[0].clone();
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
            .pixels_mut(Channel::Roughness, 128)
            .put_pixel(13, 27, Rgba([17, 83, 149, 231]));

        let events = History::stroke_recovery_events(&capture, &doc, doc.active_set_id).unwrap();
        assert_eq!(events.len(), 1);
        for event in events {
            event.apply(&mut saved).unwrap();
        }
        assert_eq!(
            saved.layers[0].pixels[&Channel::Roughness].as_raw(),
            doc.layers[0].pixels[&Channel::Roughness].as_raw()
        );
    }

    #[test]
    fn stroke_recovery_events_replay_mask_bytes_exactly() {
        let mut doc = Document::new(128);
        let mut saved = Document::new(128);
        saved.layers[0] = doc.layers[0].clone();
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
        assert_eq!(saved.layers[0].mask.as_raw(), doc.layers[0].mask.as_raw());
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
        saved.layers[0] = doc.layers[0].clone();
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
            saved.layers[0].pixels[&Channel::BaseColor].as_raw(),
            doc.layers[0].pixels[&Channel::BaseColor].as_raw()
        );
    }

    #[test]
    fn snapshot_recovery_events_replay_channel_and_mask_fill_changes() {
        let mut doc = Document::new(130);
        let mut saved = Document::new(130);
        saved.layers[0] = doc.layers[0].clone();
        let layer_id = doc.selected_layer().id;
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 130)
            .put_pixel(129, 129, image::Rgba([17, 31, 47, 255]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        let channel_snapshot = Snapshot {
            layer_id,
            channel: Channel::BaseColor,
            mask: false,
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
            saved.layers[0].pixels[&Channel::BaseColor],
            doc.layers[0].pixels[&Channel::BaseColor]
        );

        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::MaskWhite);
        doc.selected_layer_mut()
            .mask
            .put_pixel(64, 65, image::Luma([73]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::MaskWhite);
        let mask_snapshot = Snapshot {
            layer_id,
            channel: Channel::BaseColor,
            mask: true,
            before,
            after,
        };
        let mask_events =
            History::snapshot_recovery_events(&mask_snapshot, &doc, doc.active_set_id).unwrap();
        assert_eq!(mask_events.len(), 1);
        for event in mask_events {
            event.apply(&mut saved).unwrap();
        }
        assert_eq!(saved.layers[0].mask, doc.layers[0].mask);
    }

    #[test]
    fn over_limit_stroke_preserves_redo_and_rolls_back_partial_dabs() {
        let mut doc = Document::new(128);
        let mut history = History::new();
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 128)
            .put_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            before,
            after,
        });
        assert!(history.undo(&mut doc));

        let mut capture = History::begin_stroke(&doc, Channel::BaseColor, BrushMode::Paint);
        assert!(History::capture_stroke_rect(&mut capture, &doc, 1, 1, 2, 2));
        doc.selected_layer_mut()
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
        let pixels = &doc.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(pixels.get_pixel(1, 1).0, [0; 4]);
        assert_eq!(pixels.get_pixel(2, 2).0, [0; 4]);
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor]
                .get_pixel(1, 1)
                .0,
            [10, 20, 30, 255]
        );
    }

    #[test]
    fn stroke_history_survives_layer_properties_and_stack_additions() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, Rgba([90, 80, 70, 255]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            before,
            after,
        });

        let layer = doc.selected_layer_mut();
        layer.name = "Renamed".into();
        layer.visible = false;
        layer.opacity = 0.4;
        layer.blend = crate::document::BlendMode::Multiply;
        doc.add_layer().unwrap();

        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.layers[0].pixels[&Channel::BaseColor].get_pixel(3, 4).0,
            [0; 4]
        );
        assert_eq!(doc.layers[0].name, "Renamed");
        assert_eq!(doc.layers[0].opacity, 0.4);
        assert_eq!(doc.layers.len(), 2);
        assert!(history.redo(&mut doc));
        assert_eq!(
            doc.layers[0].pixels[&Channel::BaseColor].get_pixel(3, 4).0,
            [90, 80, 70, 255]
        );
    }

    #[test]
    fn remove_undo_restores_layer_then_its_prior_stroke() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(5, 6, Rgba([12, 34, 56, 255]));
        let after = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            before,
            after,
        });

        let selected_before = doc.layers[doc.selected].id;
        doc.add_layer().unwrap();
        let added_id = doc.layers[doc.selected].id;
        history.record_added_layer(&doc, added_id, selected_before, None);
        let props_before = LayerProperties::capture(doc.selected_layer());
        {
            let layer = doc.selected_layer_mut();
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
            LayerProperties::capture(doc.selected_layer()),
        );

        let selected_before_remove = doc.layers[doc.selected].id;
        let remove_index = doc.selected;
        let removed = doc.remove_layer().unwrap();
        let selected_after_remove = doc.layers[doc.selected].id;
        history.record_removed_layer(
            removed,
            remove_index,
            selected_before_remove,
            selected_after_remove,
        );

        assert!(history.undo(&mut doc));
        assert_eq!(doc.layers[doc.selected].id, added_id);
        assert_eq!(doc.selected_layer().name, "Marked layer");
        assert_eq!(doc.selected_layer().mask.get_pixel(2, 3).0, [77]);
        assert_eq!(
            doc.selected_layer().pixels[&Channel::Metallic]
                .get_pixel(2, 3)
                .0,
            [99, 99, 99, 255]
        );
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().name, "Paint Layer 2");
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layers.len(), 1);
        assert_eq!(doc.layers[0].id, 1);
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.layers[0].pixels[&Channel::BaseColor].get_pixel(5, 6).0,
            [0; 4]
        );

        for _ in 0..4 {
            assert!(history.redo(&mut doc));
        }
        assert_eq!(doc.layers.len(), 1);
        assert_eq!(doc.layers[0].id, 1);
        assert!(history.used_bytes <= MAX_HISTORY_BYTES);
    }

    #[test]
    fn property_edits_coalesce_and_restore_complete_state() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        let before = LayerProperties::capture(doc.selected_layer());
        doc.selected_layer_mut().opacity = 0.8;
        doc.selected_layer_mut().name = "First edit".into();
        let intermediate = LayerProperties::capture(doc.selected_layer());
        history.record_properties(1, before.clone(), intermediate.clone());
        doc.selected_layer_mut().opacity = 0.25;
        doc.selected_layer_mut().visible = false;
        doc.selected_layer_mut()
            .disabled_channels
            .insert(Channel::Normal);
        doc.selected_layer_mut().blend = BlendMode::Add;
        doc.selected_layer_mut().name = "Final edit".into();
        history.record_properties(
            1,
            intermediate,
            LayerProperties::capture(doc.selected_layer()),
        );

        assert_eq!(history.undo.len(), 1);
        assert!(history.undo(&mut doc));
        assert_eq!(doc.selected_layer().name, before.name);
        assert_eq!(doc.selected_layer().opacity, before.opacity);
        assert_eq!(doc.selected_layer().visible, before.visible);
        assert_eq!(doc.selected_layer().blend, before.blend);
        assert!(doc.selected_layer().disabled_channels.is_empty());
        assert!(history.redo(&mut doc));
        assert_eq!(doc.selected_layer().name, "Final edit");
        assert_eq!(doc.selected_layer().opacity, 0.25);
        assert!(!doc.selected_layer().visible);
        assert_eq!(doc.selected_layer().blend, BlendMode::Add);
        assert!(doc
            .selected_layer()
            .disabled_channels
            .contains(&Channel::Normal));
    }

    #[test]
    fn duplicate_and_reorder_restore_payload_order_and_selection() {
        let mut doc = Document::new(64);
        let mut history = History::new();
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 8, Rgba([4, 5, 6, 255]));
        doc.selected_layer_mut()
            .mask
            .put_pixel(7, 8, image::Luma([123]));

        let base_id = doc.layers[doc.selected].id;
        doc.duplicate_selected().unwrap();
        let duplicate_id = doc.layers[doc.selected].id;
        history.record_added_layer(&doc, duplicate_id, base_id, Some(base_id));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layers.len(), 1);
        assert_eq!(doc.layers[doc.selected].id, base_id);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layers[doc.selected].id, duplicate_id);
        assert_eq!(doc.layers[1].name, "Paint Layer copy");
        assert_eq!(doc.layers[1].mask.get_pixel(7, 8).0, [123]);
        assert_eq!(
            doc.layers[1].pixels[&Channel::BaseColor].get_pixel(7, 8).0,
            [4, 5, 6, 255]
        );

        doc.selected = 0;
        let before_source_edit = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 8, Rgba([200, 201, 202, 255]));
        let after_source_edit = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        history.commit(Snapshot {
            layer_id: base_id,
            channel: Channel::BaseColor,
            mask: false,
            before: before_source_edit,
            after: after_source_edit,
        });
        assert!(history.undo(&mut doc));
        assert!(history.undo(&mut doc));
        assert_eq!(doc.layers.len(), 1);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layers[1].id, duplicate_id);
        assert_eq!(
            doc.layers[1].pixels[&Channel::BaseColor].get_pixel(7, 8).0,
            [4, 5, 6, 255]
        );

        doc.add_layer().unwrap();
        let third_id = doc.layers[doc.selected].id;
        history.record_added_layer(&doc, third_id, duplicate_id, None);
        let old_order: Vec<_> = doc.layers.iter().map(|layer| layer.id).collect();
        doc.selected = 1;
        let old_index = doc.selected;
        let moved_id = doc.layers[old_index].id;
        let selected_before = moved_id;
        doc.move_selected(true);
        let new_index = doc.selected;
        let selected_after = doc.layers[doc.selected].id;
        history.record_moved_layer(
            moved_id,
            old_index,
            new_index,
            selected_before,
            selected_after,
        );
        assert!(history.undo(&mut doc));
        assert_eq!(
            doc.layers.iter().map(|layer| layer.id).collect::<Vec<_>>(),
            old_order
        );
        assert_eq!(doc.layers[doc.selected].id, moved_id);
        assert!(history.redo(&mut doc));
        assert_eq!(doc.layers[doc.selected].id, moved_id);
        assert_eq!(doc.layers[2].id, moved_id);
    }

    #[test]
    fn removed_layer_payload_limit_has_an_exact_boundary() {
        assert!(history_payload_within_limit(MAX_HISTORY_BYTES));
        assert!(!history_payload_within_limit(MAX_HISTORY_BYTES + 1));
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
        doc.layers.push(large);
        doc.selected = 1;
        doc.next_layer_id = 3;
        let mut history = History::new();
        let original_ids: Vec<_> = doc.layers.iter().map(|layer| layer.id).collect();
        let original_name = doc.selected_layer().name.clone();
        let original_mask_pixel = doc.selected_layer().mask.get_pixel(4, 5).0;

        assert!(history.remove_selected_layer(&mut doc).is_err());
        assert_eq!(doc.layers.len(), 2);
        assert_eq!(doc.selected, 1);
        assert_eq!(doc.next_layer_id, 3);
        assert_eq!(
            doc.layers.iter().map(|layer| layer.id).collect::<Vec<_>>(),
            original_ids
        );
        assert_eq!(doc.selected_layer().name, original_name);
        assert_eq!(
            doc.selected_layer().mask.get_pixel(4, 5).0,
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
        let before = History::capture(&mut doc, Channel::BaseColor, BrushMode::Paint);
        let after = before.clone();
        history.commit(Snapshot {
            layer_id: 1,
            channel: Channel::BaseColor,
            mask: false,
            before,
            after,
        });
        assert!(!history.undo(&mut doc));
    }
}

//! Bounded append-only recovery journal framing.
//!
//! This module deliberately knows nothing about documents or autosave policy. Callers
//! provide a baseline digest and serialize their own replay payloads.

use crate::document::{BlendMode, Channel, Document, MAX_DECODED_BYTES, MAX_SIZE};
use anyhow::{bail, Result};
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"KJNLv002";
const HEADER_LEN: usize = 8 + 4 + 16 + 32 + 4;
const HEADER_CHECKSUM_OFFSET: usize = HEADER_LEN - 4;
const RECORD_PREFIX_LEN: usize = 4;
const RECORD_FIXED_BODY_LEN: usize = 8 + 4 + 4;
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 1_000_000;

/// One complete, checksummed journal entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// One-based, strictly increasing sequence number.
    pub sequence: u64,
    /// Caller-owned serialized operation bytes, preserved without modification.
    pub payload: Vec<u8>,
}

/// A deterministic forward mutation that can be replayed on a validated base
/// project. Journal records hold its bounded JSON encoding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // Names mirror the layer mutations and journal payload vocabulary.
pub enum DocumentEvent {
    /// Replace one rectangle of a layer channel or mask with exact stored bytes.
    LayerImagePatch {
        set_id: u32,
        layer_id: u32,
        target: LayerImageTarget,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
    /// Replace a layer's editable name.
    LayerRename {
        set_id: u32,
        layer_id: u32,
        name: String,
    },
    /// Set a layer's visibility.
    LayerVisibility {
        set_id: u32,
        layer_id: u32,
        visible: bool,
    },
    /// Set layer opacity in the inclusive range 0..=1.
    LayerOpacity {
        set_id: u32,
        layer_id: u32,
        opacity: f32,
    },
    /// Set whether one PBR channel participates in compositing.
    LayerChannelEnabled {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
        enabled: bool,
    },
    /// Set a layer's compositing mode, including Detail normal-map blending.
    LayerBlendMode {
        set_id: u32,
        layer_id: u32,
        blend: BlendMode,
    },
    /// Move a layer to its final zero-based stack position.
    LayerReorder {
        set_id: u32,
        layer_id: u32,
        to_index: usize,
    },
    /// Select a layer by stable ID within one Texture Set.
    LayerSelection { set_id: u32, layer_id: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "channel", rename_all = "snake_case")]
pub enum LayerImageTarget {
    Channel(Channel),
    Mask,
}

impl DocumentEvent {
    /// Encode one event without allowing JSON expansion to exceed the record cap.
    pub fn encode_bounded(&self) -> io::Result<Vec<u8>> {
        let mut writer = BoundedWriter::new(MAX_PAYLOAD_BYTES);
        serde_json::to_writer(&mut writer, self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(writer.bytes)
    }

    /// Decode a bounded journal payload. Unknown event variants fail closed.
    pub fn decode_bounded(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid_data(
                "journal event exceeds the 8 MiB payload limit",
            ));
        }
        serde_json::from_slice(bytes).map_err(|error| invalid_data_owned(error.to_string()))
    }

    /// Apply a forward pixel/mask patch to a candidate document. All IDs,
    /// dimensions, target buffers and projected decoded bytes are checked first.
    pub fn apply(&self, doc: &mut Document) -> Result<()> {
        match self {
            Self::LayerRename {
                set_id,
                layer_id,
                name,
            } => {
                if name.len() > MAX_LAYER_NAME_BYTES {
                    bail!("Recovery layer name exceeds 4 KiB");
                }
                let layer = unique_layer_mut(layers_for_set(doc, *set_id)?, *layer_id)?;
                layer.name.clone_from(name);
                return Ok(());
            }
            Self::LayerVisibility {
                set_id,
                layer_id,
                visible,
            } => {
                unique_layer_mut(layers_for_set(doc, *set_id)?, *layer_id)?.visible = *visible;
                return Ok(());
            }
            Self::LayerOpacity {
                set_id,
                layer_id,
                opacity,
            } => {
                if !opacity.is_finite() || !(0.0..=1.0).contains(opacity) {
                    bail!("Recovery layer opacity must be finite and within 0..=1");
                }
                unique_layer_mut(layers_for_set(doc, *set_id)?, *layer_id)?.opacity = *opacity;
                return Ok(());
            }
            Self::LayerChannelEnabled {
                set_id,
                layer_id,
                channel,
                enabled,
            } => {
                let layer = unique_layer_mut(layers_for_set(doc, *set_id)?, *layer_id)?;
                if *enabled {
                    layer.disabled_channels.remove(channel);
                } else {
                    layer.disabled_channels.insert(*channel);
                }
                return Ok(());
            }
            Self::LayerBlendMode {
                set_id,
                layer_id,
                blend,
            } => {
                unique_layer_mut(layers_for_set(doc, *set_id)?, *layer_id)?.blend = *blend;
                return Ok(());
            }
            Self::LayerReorder {
                set_id,
                layer_id,
                to_index,
            } => {
                let (layers, selected) = layers_and_selection_for_set(doc, *set_id)?;
                let from_index = unique_layer_index(layers, *layer_id)?;
                if *to_index >= layers.len() {
                    bail!("Recovery layer destination index is out of range");
                }
                if *selected >= layers.len() {
                    bail!("Recovery selected layer index is out of range");
                }
                let selected_id = layers[*selected].id;
                let layer = layers.remove(from_index);
                layers.insert(*to_index, layer);
                *selected = unique_layer_index(layers, selected_id)?;
                return Ok(());
            }
            Self::LayerSelection { set_id, layer_id } => {
                let (layers, selected) = layers_and_selection_for_set(doc, *set_id)?;
                *selected = unique_layer_index(layers, *layer_id)?;
                return Ok(());
            }
            Self::LayerImagePatch { .. } => {}
        }
        let Self::LayerImagePatch {
            set_id,
            layer_id,
            target,
            x,
            y,
            width,
            height,
            bytes,
        } = self
        else {
            unreachable!("all structural recovery variants return from the match above")
        };
        let decoded_bytes_before = doc.decoded_bytes();
        let (layers, size) = if *set_id == doc.active_set_id {
            (&mut doc.layers, doc.size)
        } else {
            let set = doc
                .texture_sets
                .iter_mut()
                .find(|set| set.id == *set_id)
                .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?;
            (&mut set.layers, set.size)
        };
        if size == 0 || size > MAX_SIZE || *width == 0 || *height == 0 {
            bail!("Invalid recovery image patch dimensions");
        }
        let right = x
            .checked_add(*width)
            .ok_or_else(|| anyhow::anyhow!("Recovery patch X coordinate overflow"))?;
        let bottom = y
            .checked_add(*height)
            .ok_or_else(|| anyhow::anyhow!("Recovery patch Y coordinate overflow"))?;
        if right > size || bottom > size {
            bail!("Recovery image patch is outside its Texture Set");
        }
        let bytes_per_pixel = match target {
            LayerImageTarget::Channel(_) => 4u64,
            LayerImageTarget::Mask => 1u64,
        };
        let expected_bytes = u64::from(*width)
            .checked_mul(u64::from(*height))
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or_else(|| anyhow::anyhow!("Recovery patch byte size overflow"))?;
        if expected_bytes != bytes.len() as u64 {
            bail!("Recovery patch byte length does not match its dimensions");
        }
        let layer = layers
            .iter_mut()
            .find(|layer| layer.id == *layer_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown recovery layer {layer_id}"))?;
        match target {
            LayerImageTarget::Mask => {
                if layer.mask.dimensions() != (size, size)
                    || layer.mask.as_raw().len() != (size as usize).saturating_mul(size as usize)
                {
                    bail!("Recovery target mask dimensions do not match its Texture Set");
                }
                let target = layer.mask.as_mut();
                copy_patch_rows(
                    target,
                    size,
                    PatchRect {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                    },
                    1,
                    bytes,
                )?;
            }
            LayerImageTarget::Channel(channel) => {
                if let Some(image) = layer.pixels.get(channel) {
                    let expected_len = (size as usize)
                        .checked_mul(size as usize)
                        .and_then(|pixels| pixels.checked_mul(4))
                        .ok_or_else(|| anyhow::anyhow!("Recovery channel size overflow"))?;
                    if image.dimensions() != (size, size) || image.as_raw().len() != expected_len {
                        bail!("Recovery target channel dimensions do not match its Texture Set");
                    }
                } else {
                    let full_bytes = u64::from(size)
                        .checked_mul(u64::from(size))
                        .and_then(|pixels| pixels.checked_mul(4))
                        .ok_or_else(|| anyhow::anyhow!("Recovery channel size overflow"))?;
                    if !channel_allocation_fits(decoded_bytes_before, full_bytes) {
                        bail!("Recovery patch exceeds the 512-MiB decoded texture budget");
                    }
                    let allocation = usize::try_from(full_bytes)
                        .map_err(|_| anyhow::anyhow!("Recovery channel size is not addressable"))?;
                    let mut pixels = Vec::new();
                    pixels.try_reserve_exact(allocation)?;
                    pixels.resize(allocation, 0);
                    let image = RgbaImage::from_raw(size, size, pixels)
                        .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery channel"))?;
                    layer.pixels.insert(*channel, image);
                }
                let target = layer.pixels.get_mut(channel).unwrap().as_mut();
                copy_patch_rows(
                    target,
                    size,
                    PatchRect {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                    },
                    4,
                    bytes,
                )?;
            }
        }
        Ok(())
    }
}

const MAX_LAYER_NAME_BYTES: usize = 4096;

fn layers_for_set(doc: &mut Document, set_id: u32) -> Result<&mut Vec<crate::document::Layer>> {
    if set_id == 0 {
        bail!("Recovery Texture Set ID must be nonzero");
    }
    if set_id == doc.active_set_id {
        Ok(&mut doc.layers)
    } else {
        Ok(&mut doc
            .texture_sets
            .iter_mut()
            .find(|set| set.id == set_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?
            .layers)
    }
}

fn layers_and_selection_for_set(
    doc: &mut Document,
    set_id: u32,
) -> Result<(&mut Vec<crate::document::Layer>, &mut usize)> {
    if set_id == 0 {
        bail!("Recovery Texture Set ID must be nonzero");
    }
    if set_id == doc.active_set_id {
        Ok((&mut doc.layers, &mut doc.selected))
    } else {
        let set = doc
            .texture_sets
            .iter_mut()
            .find(|set| set.id == set_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?;
        Ok((&mut set.layers, &mut set.selected))
    }
}

fn unique_layer_index(layers: &[crate::document::Layer], layer_id: u32) -> Result<usize> {
    if layer_id == 0 {
        bail!("Recovery layer ID must be nonzero");
    }
    let mut found = None;
    for (index, layer) in layers.iter().enumerate() {
        if layer.id == layer_id {
            if found.is_some() {
                bail!("Recovery layer ID is ambiguous");
            }
            found = Some(index);
        }
    }
    found.ok_or_else(|| anyhow::anyhow!("Unknown recovery layer {layer_id}"))
}

fn unique_layer_mut(
    layers: &mut [crate::document::Layer],
    layer_id: u32,
) -> Result<&mut crate::document::Layer> {
    let index = unique_layer_index(layers, layer_id)?;
    Ok(&mut layers[index])
}

/// Encode an ordered batch as one JSON array, keeping the entire atomic record
/// within the journal payload limit.
pub fn encode_event_batch_bounded(events: &[DocumentEvent]) -> io::Result<Vec<u8>> {
    let mut writer = BoundedWriter::new(MAX_PAYLOAD_BYTES);
    serde_json::to_writer(&mut writer, events)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(writer.bytes)
}

/// Decode one bounded JSON event array. Unknown variants and malformed events
/// fail closed so a partially understood record cannot be replayed.
pub fn decode_event_batch_bounded(bytes: &[u8]) -> io::Result<Vec<DocumentEvent>> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(invalid_data(
            "journal event batch exceeds the 8 MiB payload limit",
        ));
    }
    serde_json::from_slice(bytes).map_err(|error| invalid_data_owned(error.to_string()))
}

fn channel_allocation_fits(decoded_bytes_before: u64, allocation_bytes: u64) -> bool {
    decoded_bytes_before
        .checked_add(allocation_bytes)
        .is_some_and(|used| used <= MAX_DECODED_BYTES)
}

struct PatchRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn copy_patch_rows(
    target: &mut [u8],
    size: u32,
    rect: PatchRect,
    bytes_per_pixel: usize,
    bytes: &[u8],
) -> Result<()> {
    let row_bytes = usize::try_from(rect.width)
        .ok()
        .and_then(|width| width.checked_mul(bytes_per_pixel))
        .ok_or_else(|| anyhow::anyhow!("Recovery patch row size overflow"))?;
    let stride = usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_mul(bytes_per_pixel))
        .ok_or_else(|| anyhow::anyhow!("Recovery image stride overflow"))?;
    let x_bytes = usize::try_from(rect.x)
        .ok()
        .and_then(|x| x.checked_mul(bytes_per_pixel))
        .ok_or_else(|| anyhow::anyhow!("Recovery patch offset overflow"))?;
    for row in 0..rect.height as usize {
        let target_start = usize::try_from(rect.y)
            .ok()
            .and_then(|y| y.checked_add(row))
            .and_then(|y| y.checked_mul(stride))
            .and_then(|offset| offset.checked_add(x_bytes))
            .ok_or_else(|| anyhow::anyhow!("Recovery patch target offset overflow"))?;
        let source_start = row
            .checked_mul(row_bytes)
            .ok_or_else(|| anyhow::anyhow!("Recovery patch source offset overflow"))?;
        let target_row = target
            .get_mut(target_start..target_start + row_bytes)
            .ok_or_else(|| anyhow::anyhow!("Recovery patch exceeds target image storage"))?;
        let source_row = bytes
            .get(source_start..source_start + row_bytes)
            .ok_or_else(|| anyhow::anyhow!("Recovery patch exceeds source data"))?;
        target_row.copy_from_slice(source_row);
    }
    Ok(())
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let new_len = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|len| *len <= self.limit)
            .ok_or_else(|| invalid_input("journal event exceeds the 8 MiB payload limit"))?;
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| io::Error::other("could not reserve bounded journal event memory"))?;
        self.bytes.extend_from_slice(bytes);
        debug_assert_eq!(self.bytes.len(), new_len);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An open journal ready for append. Append calls are serialized through this handle.
pub struct Journal {
    file: File,
    next_sequence: u64,
    bytes_written: u64,
    poisoned: bool,
}

impl Journal {
    /// Create a new journal without replacing an existing path.
    ///
    /// The header is synced before this function returns.
    pub fn create(
        path: impl AsRef<Path>,
        project_id: Uuid,
        baseline_digest: [u8; 32],
        schema_version: u32,
    ) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let header = encode_header(project_id, baseline_digest, schema_version);
        file.write_all(&header)?;
        file.flush()?;
        file.sync_all()?;
        Ok(Self {
            file,
            next_sequence: 1,
            bytes_written: HEADER_LEN as u64,
            poisoned: false,
        })
    }

    /// Open and validate an existing journal, returning all complete records.
    ///
    /// A truncated final frame is ignored. Any complete malformed frame is rejected.
    pub fn open(
        path: impl AsRef<Path>,
        project_id: Uuid,
        baseline_digest: [u8; 32],
        schema_version: u32,
    ) -> io::Result<(Self, Vec<Record>)> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let (records, valid_len) =
            scan_file(&mut file, project_id, baseline_digest, schema_version)?;
        let original_len = file.metadata()?.len();
        if valid_len < original_len {
            file.set_len(valid_len)?;
            file.sync_all()?;
        }
        let bytes_written = valid_len;
        file.seek(SeekFrom::End(0))?;
        let next_sequence = records
            .last()
            .map_or(Some(1), |record| record.sequence.checked_add(1))
            .ok_or_else(|| invalid_data("journal sequence number overflow"))?;
        Ok((
            Self {
                file,
                next_sequence,
                bytes_written,
                poisoned: false,
            },
            records,
        ))
    }

    /// Append and durably sync one payload, returning its sequence number.
    ///
    /// If writing or syncing fails, the handle is poisoned and cannot append again;
    /// reopen and scan it to determine whether the final frame committed.
    pub fn append(&mut self, payload: &[u8]) -> io::Result<u64> {
        if self.poisoned {
            return Err(io::Error::other(
                "journal handle is poisoned after a prior write failure",
            ));
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid_input("journal payload exceeds 8 MiB"));
        }
        let frame_len =
            RECORD_PREFIX_LEN as u64 + RECORD_FIXED_BODY_LEN as u64 + payload.len() as u64;
        let new_size = self
            .bytes_written
            .checked_add(frame_len)
            .ok_or_else(|| invalid_input("journal size overflow"))?;
        if new_size > MAX_JOURNAL_BYTES {
            return Err(invalid_input("journal exceeds 256 MiB replay limit"));
        }

        let sequence = self.next_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| invalid_input("journal sequence number overflow"))?;
        let body_len = 8 + 4 + payload.len() + 4;
        let mut checksum_input = Vec::new();
        checksum_input
            .try_reserve_exact(8 + 4 + payload.len())
            .map_err(|_| io::Error::other("could not reserve journal frame memory"))?;
        checksum_input.extend_from_slice(&sequence.to_le_bytes());
        checksum_input.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        checksum_input.extend_from_slice(payload);
        let checksum = crc32(&checksum_input);

        let result = (|| {
            self.file.write_all(&(body_len as u32).to_le_bytes())?;
            self.file.write_all(&checksum_input)?;
            self.file.write_all(&checksum.to_le_bytes())?;
            self.file.flush()?;
            self.file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            self.poisoned = true;
            return Err(error);
        }

        self.bytes_written = new_size;
        self.next_sequence = next_sequence;
        Ok(sequence)
    }
}

/// Scan and validate a journal without opening it for append.
pub fn scan(
    path: impl AsRef<Path>,
    project_id: Uuid,
    baseline_digest: [u8; 32],
    schema_version: u32,
) -> io::Result<Vec<Record>> {
    let mut file = File::open(path)?;
    scan_file(&mut file, project_id, baseline_digest, schema_version).map(|(records, _)| records)
}

/// Hard upper bound on one payload, useful to callers when preparing records.
pub const fn max_payload_bytes() -> usize {
    MAX_PAYLOAD_BYTES
}

/// Hard upper bound on bytes scanned or replayed, including the header.
pub const fn max_journal_bytes() -> u64 {
    MAX_JOURNAL_BYTES
}

fn scan_file(
    file: &mut File,
    expected_project_id: Uuid,
    expected_digest: [u8; 32],
    expected_schema: u32,
) -> io::Result<(Vec<Record>, u64)> {
    let size = file.metadata()?.len();
    if size > MAX_JOURNAL_BYTES {
        return Err(invalid_data("journal exceeds 256 MiB scan limit"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; HEADER_LEN];
    if read_up_to(file, &mut header)? != HEADER_LEN {
        return Err(invalid_data("journal header is truncated"));
    }
    validate_header(
        &header,
        expected_project_id,
        expected_digest,
        expected_schema,
    )?;

    let mut records = Vec::new();
    let mut cursor = HEADER_LEN;
    let mut expected_sequence = 1u64;
    loop {
        let mut prefix = [0u8; RECORD_PREFIX_LEN];
        let prefix_read = read_up_to(file, &mut prefix)?;
        if prefix_read == 0 {
            break;
        }
        if prefix_read < RECORD_PREFIX_LEN {
            break; // only a truncated final length prefix may be ignored
        }
        let body_len = u32::from_le_bytes(prefix) as usize;
        if body_len < RECORD_FIXED_BODY_LEN {
            return Err(invalid_data(
                "journal record is shorter than its fixed fields",
            ));
        }
        let payload_len = body_len - RECORD_FIXED_BODY_LEN;
        if payload_len > MAX_PAYLOAD_BYTES {
            return Err(invalid_data("journal record payload exceeds 8 MiB"));
        }
        let frame_len = RECORD_PREFIX_LEN as u64 + body_len as u64;
        let next_cursor = (cursor as u64)
            .checked_add(frame_len)
            .ok_or_else(|| invalid_data("journal size overflow"))?;
        if next_cursor > MAX_JOURNAL_BYTES {
            return Err(invalid_data("journal exceeds 256 MiB scan limit"));
        }
        let mut body = Vec::new();
        body.try_reserve_exact(body_len)
            .map_err(|_| io::Error::other("could not reserve journal scan memory"))?;
        body.resize(body_len, 0);
        if read_up_to(file, &mut body)? < body_len {
            break; // the only tolerated incomplete record is the final frame
        }

        let sequence = u64::from_le_bytes(body[..8].try_into().expect("fixed-width slice"));
        if sequence != expected_sequence {
            return Err(invalid_data(
                "journal records are duplicate or out of sequence",
            ));
        }
        let stored_payload_len =
            u32::from_le_bytes(body[8..12].try_into().expect("fixed-width slice")) as usize;
        if stored_payload_len != payload_len {
            return Err(invalid_data("journal record length fields disagree"));
        }
        let payload_end = 12 + payload_len;
        let stored_crc = u32::from_le_bytes(
            body[payload_end..payload_end + 4]
                .try_into()
                .expect("fixed-width slice"),
        );
        if crc32(&body[..payload_end]) != stored_crc {
            return Err(invalid_data("journal record checksum mismatch"));
        }
        if records.len() >= MAX_RECORDS {
            return Err(invalid_data("journal exceeds the 1,000,000 record limit"));
        }
        records
            .try_reserve(1)
            .map_err(|_| io::Error::other("could not reserve journal record index memory"))?;
        body.drain(..12);
        body.truncate(payload_len);
        records.push(Record {
            sequence,
            payload: body,
        });
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("journal sequence number overflow"))?;
        cursor = next_cursor as usize;
    }
    Ok((records, cursor as u64))
}

fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(read)
}

fn encode_header(project_id: Uuid, digest: [u8; 32], schema_version: u32) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&schema_version.to_le_bytes());
    header[12..28].copy_from_slice(project_id.as_bytes());
    header[28..60].copy_from_slice(&digest);
    let checksum = crc32(&header[..HEADER_CHECKSUM_OFFSET]);
    header[HEADER_CHECKSUM_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn validate_header(
    header: &[u8],
    expected_project_id: Uuid,
    expected_digest: [u8; 32],
    expected_schema: u32,
) -> io::Result<()> {
    if &header[..8] != MAGIC {
        return Err(invalid_data("journal magic or format version is invalid"));
    }
    let stored_schema = u32::from_le_bytes(header[8..12].try_into().expect("fixed-width slice"));
    if stored_schema != expected_schema {
        return Err(invalid_data("journal schema version mismatch"));
    }
    if header[12..28] != expected_project_id.as_bytes()[..] {
        return Err(invalid_data("journal project identity mismatch"));
    }
    if header[28..60] != expected_digest {
        return Err(invalid_data("journal baseline digest mismatch"));
    }
    let stored_crc = u32::from_le_bytes(
        header[HEADER_CHECKSUM_OFFSET..]
            .try_into()
            .expect("fixed-width slice"),
    );
    if crc32(&header[..HEADER_CHECKSUM_OFFSET]) != stored_crc {
        return Err(invalid_data("journal header checksum mismatch"));
    }
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_data_owned(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    const DIGEST: [u8; 32] = [0x5a; 32];
    const SCHEMA: u32 = 7;
    const PROJECT_ID: Uuid = Uuid::from_u128(0x83cfd72081e94ef09f66d2a3c4b5a617);

    struct TempPath(std::path::PathBuf);

    impl TempPath {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "kalium-recovery-{label}-{}-{nonce}.journal",
                std::process::id()
            )))
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn create_with_payloads(path: &Path, payloads: &[&[u8]]) {
        let mut journal =
            Journal::create(path, PROJECT_ID, DIGEST, SCHEMA).expect("create journal");
        for payload in payloads {
            journal.append(payload).expect("append payload");
        }
    }

    #[test]
    fn image_patch_event_roundtrips_and_replays_channel_and_mask_bytes() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().id;
        let channel_event = DocumentEvent::LayerImagePatch {
            set_id: doc.active_set_id,
            layer_id,
            target: LayerImageTarget::Channel(Channel::BaseColor),
            x: 7,
            y: 9,
            width: 2,
            height: 2,
            bytes: vec![
                11, 22, 33, 44, 51, 52, 53, 54, 61, 62, 63, 64, 71, 72, 73, 74,
            ],
        };
        let encoded = channel_event.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        decoded.apply(&mut doc).unwrap();
        let image = &doc.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(image.get_pixel(7, 9).0, [11, 22, 33, 44]);
        assert_eq!(image.get_pixel(8, 10).0, [71, 72, 73, 74]);

        let mask_event = DocumentEvent::LayerImagePatch {
            set_id: doc.active_set_id,
            layer_id,
            target: LayerImageTarget::Mask,
            x: 8,
            y: 9,
            width: 2,
            height: 2,
            bytes: vec![3, 4, 5, 6],
        };
        mask_event.apply(&mut doc).unwrap();
        assert_eq!(doc.selected_layer().mask.get_pixel(8, 9).0, [3]);
        assert_eq!(doc.selected_layer().mask.get_pixel(9, 10).0, [6]);
    }

    #[test]
    fn structural_events_roundtrip_and_replay_exact_layer_state() {
        let mut doc = Document::new(64);
        let first_id = doc.layers[0].id;
        doc.add_layer().unwrap();
        let second_id = doc.layers[1].id;
        let events = vec![
            DocumentEvent::LayerRename {
                set_id: doc.active_set_id,
                layer_id: first_id,
                name: "Paint • details".into(),
            },
            DocumentEvent::LayerVisibility {
                set_id: doc.active_set_id,
                layer_id: second_id,
                visible: false,
            },
            DocumentEvent::LayerOpacity {
                set_id: doc.active_set_id,
                layer_id: first_id,
                opacity: 0.375,
            },
            DocumentEvent::LayerChannelEnabled {
                set_id: doc.active_set_id,
                layer_id: second_id,
                channel: Channel::Metallic,
                enabled: false,
            },
            DocumentEvent::LayerReorder {
                set_id: doc.active_set_id,
                layer_id: second_id,
                to_index: 0,
            },
        ];
        let encoded = encode_event_batch_bounded(&events).unwrap();
        let decoded = decode_event_batch_bounded(&encoded).unwrap();
        assert_eq!(decoded, events);
        for event in decoded {
            event.apply(&mut doc).unwrap();
        }

        assert_eq!(
            doc.layers.iter().map(|layer| layer.id).collect::<Vec<_>>(),
            [second_id, first_id]
        );
        assert_eq!(doc.layers[1].name, "Paint • details");
        assert_eq!(doc.layers[1].opacity, 0.375);
        assert!(!doc.layers[0].visible);
        assert!(doc.layers[0].disabled_channels.contains(&Channel::Metallic));
        assert_eq!(
            doc.selected, 0,
            "reorder preserves selection by layer identity"
        );
    }

    #[test]
    fn structural_events_reject_invalid_ids_indices_and_values_without_mutation() {
        let mut doc = Document::new(64);
        let layer_id = doc.layers[0].id;
        doc.add_layer().unwrap();
        let before = doc.layers.iter().map(|layer| layer.id).collect::<Vec<_>>();

        let bad_events = [
            DocumentEvent::LayerRename {
                set_id: 0,
                layer_id,
                name: "invalid set".into(),
            },
            DocumentEvent::LayerVisibility {
                set_id: doc.active_set_id,
                layer_id: 0,
                visible: false,
            },
            DocumentEvent::LayerOpacity {
                set_id: doc.active_set_id,
                layer_id,
                opacity: f32::NAN,
            },
            DocumentEvent::LayerOpacity {
                set_id: doc.active_set_id,
                layer_id,
                opacity: 1.01,
            },
            DocumentEvent::LayerReorder {
                set_id: doc.active_set_id,
                layer_id,
                to_index: 2,
            },
            DocumentEvent::LayerRename {
                set_id: doc.active_set_id,
                layer_id,
                name: "x".repeat(MAX_LAYER_NAME_BYTES + 1),
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.layers.iter().map(|layer| layer.id).collect::<Vec<_>>(),
                before
            );
        }
        assert_eq!(doc.layers[0].name, "Paint Layer");
        assert_eq!(doc.layers[0].opacity, 1.0);
    }

    #[test]
    fn layer_blend_mode_roundtrips_and_replays_every_mode() {
        let mut doc = Document::new(64);
        let layer_id = doc.layers[0].id;

        for blend in BlendMode::ALL {
            let event = DocumentEvent::LayerBlendMode {
                set_id: doc.active_set_id,
                layer_id,
                blend,
            };
            let encoded = event.encode_bounded().unwrap();
            let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
            assert_eq!(decoded, event);
            decoded.apply(&mut doc).unwrap();
            assert_eq!(doc.layers[0].blend, blend);
        }
    }

    #[test]
    fn image_patch_replay_supports_inactive_texture_sets() {
        let mut doc = Document::new(64);
        let set_id = doc.add_texture_set("Recovery Set").unwrap();
        doc.activate_texture_set(1).unwrap();
        let layer_id = doc
            .texture_sets
            .iter()
            .find(|set| set.id == set_id)
            .unwrap()
            .layers[0]
            .id;
        let event = DocumentEvent::LayerImagePatch {
            set_id,
            layer_id,
            target: LayerImageTarget::Channel(Channel::Roughness),
            x: 1,
            y: 2,
            width: 1,
            height: 1,
            bytes: vec![120, 0, 0, 255],
        };
        event.apply(&mut doc).unwrap();
        assert_eq!(
            doc.texture_sets
                .iter()
                .find(|set| set.id == set_id)
                .unwrap()
                .layers[0]
                .pixels[&Channel::Roughness]
                .get_pixel(1, 2)
                .0,
            [120, 0, 0, 255]
        );
        assert!(!doc.layers[0].pixels.contains_key(&Channel::Roughness));
    }

    #[test]
    fn image_patch_replay_rejects_invalid_rectangles_without_mutation() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().id;
        let invalid = DocumentEvent::LayerImagePatch {
            set_id: doc.active_set_id,
            layer_id,
            target: LayerImageTarget::Channel(Channel::BaseColor),
            x: 63,
            y: 0,
            width: 2,
            height: 1,
            bytes: vec![0; 8],
        };
        assert!(invalid.apply(&mut doc).is_err());
        assert!(!doc
            .selected_layer()
            .pixels
            .contains_key(&Channel::BaseColor));

        let missing_layer = DocumentEvent::LayerImagePatch {
            set_id: doc.active_set_id,
            layer_id: layer_id + 500,
            target: LayerImageTarget::Mask,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![0],
        };
        assert!(missing_layer.apply(&mut doc).is_err());
    }

    #[test]
    fn image_patch_replay_rejects_malformed_existing_targets_without_mutation() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().id;
        doc.selected_layer_mut()
            .pixels
            .insert(Channel::BaseColor, RgbaImage::new(1, 1));
        let before_channel = doc.selected_layer().pixels[&Channel::BaseColor].clone();
        let bad_channel = DocumentEvent::LayerImagePatch {
            set_id: doc.active_set_id,
            layer_id,
            target: LayerImageTarget::Channel(Channel::BaseColor),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![1, 2, 3, 4],
        };
        assert!(bad_channel.apply(&mut doc).is_err());
        assert_eq!(
            doc.selected_layer().pixels[&Channel::BaseColor],
            before_channel
        );

        doc.selected_layer_mut().mask = image::GrayImage::new(1, 1);
        let before_mask = doc.selected_layer().mask.clone();
        let bad_mask = DocumentEvent::LayerImagePatch {
            set_id: doc.active_set_id,
            layer_id,
            target: LayerImageTarget::Mask,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![7],
        };
        assert!(bad_mask.apply(&mut doc).is_err());
        assert_eq!(doc.selected_layer().mask, before_mask);
    }

    #[test]
    fn absent_channel_allocation_guard_accepts_exact_budget_and_rejects_overflow() {
        let allocation = 4 * 64 * 64;
        assert!(channel_allocation_fits(
            MAX_DECODED_BYTES - allocation,
            allocation
        ));
        assert!(!channel_allocation_fits(
            MAX_DECODED_BYTES - allocation + 1,
            allocation
        ));
        assert!(!channel_allocation_fits(u64::MAX, 1));
    }

    #[test]
    fn event_json_expansion_cannot_exceed_record_payload_cap() {
        let event = DocumentEvent::LayerImagePatch {
            set_id: 1,
            layer_id: 1,
            target: LayerImageTarget::Channel(Channel::BaseColor),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![127; 3 * 1024 * 1024],
        };
        assert_eq!(
            event.encode_bounded().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn event_batch_codec_roundtrips_ordered_events_and_empty_batches() {
        let events = vec![
            DocumentEvent::LayerImagePatch {
                set_id: 3,
                layer_id: 7,
                target: LayerImageTarget::Channel(Channel::BaseColor),
                x: 1,
                y: 2,
                width: 1,
                height: 1,
                bytes: vec![11, 22, 33, 44],
            },
            DocumentEvent::LayerImagePatch {
                set_id: 3,
                layer_id: 9,
                target: LayerImageTarget::Mask,
                x: 5,
                y: 6,
                width: 2,
                height: 1,
                bytes: vec![8, 9],
            },
        ];

        let encoded = encode_event_batch_bounded(&events).unwrap();
        assert_eq!(decode_event_batch_bounded(&encoded).unwrap(), events);

        let empty = encode_event_batch_bounded(&[]).unwrap();
        assert_eq!(empty, b"[]");
        assert!(decode_event_batch_bounded(&empty).unwrap().is_empty());
    }

    #[test]
    fn event_batch_codec_rejects_over_cap_and_malformed_or_unknown_events() {
        let oversized_event = DocumentEvent::LayerImagePatch {
            set_id: 1,
            layer_id: 1,
            target: LayerImageTarget::Channel(Channel::BaseColor),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![127; 3 * 1024 * 1024],
        };
        assert_eq!(
            encode_event_batch_bounded(&[oversized_event])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let oversized_input = vec![b' '; MAX_PAYLOAD_BYTES + 1];
        assert_eq!(
            decode_event_batch_bounded(&oversized_input)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            decode_event_batch_bounded(br#"[{"event":"future_event"}]"#)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            decode_event_batch_bounded(br#"[{"event":"layer_image_patch"}]"#)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn round_trip_preserves_payloads_and_sequence_numbers() {
        let path = TempPath::new("round-trip");
        let payloads: &[&[u8]] = &[b"stroke\0bytes", &[0, 255, 1, 128], b""];
        create_with_payloads(&path.0, payloads);
        let records = scan(&path.0, PROJECT_ID, DIGEST, SCHEMA).expect("scan journal");
        assert_eq!(
            records,
            payloads
                .iter()
                .enumerate()
                .map(|(index, payload)| Record {
                    sequence: index as u64 + 1,
                    payload: payload.to_vec(),
                })
                .collect::<Vec<_>>()
        );

        let (mut journal, prior) =
            Journal::open(&path.0, PROJECT_ID, DIGEST, SCHEMA).expect("open journal");
        assert_eq!(prior, records);
        assert_eq!(journal.append(b"after reopen").expect("append"), 4);
        assert_eq!(scan(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap().len(), 4);
    }

    #[test]
    fn rejects_identity_and_schema_mismatch() {
        let path = TempPath::new("identity");
        create_with_payloads(&path.0, &[b"one"]);
        assert_eq!(
            scan(
                &path.0,
                Uuid::from_u128(0x993d1da245ea4b008fb1989f84e6ba11),
                DIGEST,
                SCHEMA
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(scan(&path.0, PROJECT_ID, [0x33; 32], SCHEMA).is_err());
        assert!(scan(&path.0, PROJECT_ID, DIGEST, SCHEMA + 1).is_err());
    }

    #[test]
    fn rejects_checksum_corruption_in_complete_record() {
        let path = TempPath::new("checksum");
        create_with_payloads(&path.0, &[b"valid payload"]);
        let mut bytes = fs::read(&path.0).unwrap();
        *bytes.last_mut().unwrap() ^= 0x80;
        fs::write(&path.0, bytes).unwrap();
        assert_eq!(
            scan(&path.0, PROJECT_ID, DIGEST, SCHEMA)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn ignores_only_truncated_final_frame() {
        let path = TempPath::new("truncated");
        create_with_payloads(&path.0, &[b"committed", b"tail to truncate"]);
        let mut bytes = fs::read(&path.0).unwrap();
        bytes.truncate(bytes.len() - 5);
        fs::write(&path.0, bytes).unwrap();
        let records = scan(&path.0, PROJECT_ID, DIGEST, SCHEMA).expect("truncated tail ignored");
        assert_eq!(
            records,
            vec![Record {
                sequence: 1,
                payload: b"committed".to_vec()
            }]
        );
    }

    #[test]
    fn open_truncates_incomplete_tail_before_next_append() {
        let path = TempPath::new("truncate-then-append");
        create_with_payloads(&path.0, &[b"committed", b"incomplete"]);
        let mut bytes = fs::read(&path.0).unwrap();
        bytes.truncate(bytes.len() - 3);
        fs::write(&path.0, bytes).unwrap();
        let (mut journal, records) = Journal::open(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(journal.append(b"next").unwrap(), 2);
        assert_eq!(
            scan(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap(),
            vec![
                Record {
                    sequence: 1,
                    payload: b"committed".to_vec()
                },
                Record {
                    sequence: 2,
                    payload: b"next".to_vec()
                },
            ]
        );
    }

    #[test]
    fn rejects_duplicate_or_out_of_order_sequences() {
        let path = TempPath::new("sequence");
        create_with_payloads(&path.0, &[b"first", b"second"]);
        let mut bytes = fs::read(&path.0).unwrap();
        let second_sequence_offset =
            HEADER_LEN + (RECORD_PREFIX_LEN + 8 + 4 + 5 + 4) + RECORD_PREFIX_LEN;
        bytes[second_sequence_offset..second_sequence_offset + 8]
            .copy_from_slice(&1u64.to_le_bytes());
        // Refresh the second record checksum, leaving a structurally valid duplicate sequence.
        let second_body_start = second_sequence_offset;
        let second_payload_len_offset = second_body_start + 8;
        let payload_len = u32::from_le_bytes(
            bytes[second_payload_len_offset..second_payload_len_offset + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let payload_end = second_body_start + 12 + payload_len;
        let crc = crc32(&bytes[second_body_start..payload_end]);
        bytes[payload_end..payload_end + 4].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path.0, bytes).unwrap();
        assert_eq!(
            scan(&path.0, PROJECT_ID, DIGEST, SCHEMA)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn enforces_payload_and_total_scan_limits() {
        let path = TempPath::new("limits");
        let mut journal = Journal::create(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        assert_eq!(
            journal
                .append(&vec![0; MAX_PAYLOAD_BYTES + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        drop(journal);

        let boundary = TempPath::new("payload-boundary");
        let mut journal = Journal::create(&boundary.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        let payload = vec![0xa5; MAX_PAYLOAD_BYTES];
        journal.append(&payload).expect("8 MiB payload is accepted");
        assert_eq!(
            scan(&boundary.0, PROJECT_ID, DIGEST, SCHEMA).unwrap()[0].payload,
            payload
        );

        let oversized = TempPath::new("oversized");
        File::create(&oversized.0)
            .unwrap()
            .set_len(MAX_JOURNAL_BYTES + 1)
            .unwrap();
        assert_eq!(
            scan(&oversized.0, PROJECT_ID, DIGEST, SCHEMA)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn refuses_append_that_would_cross_replay_limit() {
        let path = TempPath::new("append-limit");
        let mut journal = Journal::create(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        journal.bytes_written = MAX_JOURNAL_BYTES - 10;
        assert_eq!(
            journal.append(b"payload").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

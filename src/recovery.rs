//! Bounded append-only recovery journal framing.
//!
//! This module deliberately knows nothing about documents or autosave policy. Callers
//! provide a baseline digest and serialize their own replay payloads.

use crate::document::{
    BlendMode, Channel, Document, Layer, TextureSet, MAX_DECODED_BYTES, MAX_LAYERS, MAX_SIZE,
    MAX_TEXTURE_SETS,
};
use crate::layer_tree::{
    FillLayer, LayerGroup, LayerNode, LayerNodeProperties, LayerTree, MAX_LAYER_TREE_DEPTH,
    MAX_LAYER_TREE_NODES,
};
use crate::procedural::ProceduralGraph;
use anyhow::{bail, Result};
use image::{GrayImage, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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
// A loaded project's entire manifest is capped at 48 MiB. This local cap
// bounds extension cloning if a caller constructs an in-memory document.
const MAX_LAYER_EXTENSION_BYTES: usize = 48 * 1024 * 1024;

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
    /// Insert a new blank layer at its final zero-based stack position.
    LayerCreate {
        set_id: u32,
        layer_id: u32,
        to_index: usize,
        name: String,
    },
    /// Duplicate the selected layer, preserving its full editable state.
    LayerDuplicate {
        set_id: u32,
        source_layer_id: u32,
        layer_id: u32,
        to_index: usize,
    },
    /// Reinsert a layer previously allocated and removed during this journal
    /// replay. A duplicate reinsert copies the selected source layer's current
    /// editable state; a blank reinsert allocates only its mask.
    LayerReinsert {
        set_id: u32,
        layer_id: u32,
        to_index: usize,
        name: String,
        source_layer_id: Option<u32>,
    },
    /// Remove the selected layer and choose another layer by stable ID.
    LayerRemove {
        set_id: u32,
        layer_id: u32,
        selected_after_id: u32,
    },
    /// Rename one Texture Set after trimming surrounding Unicode whitespace.
    TextureSetRename { set_id: u32, name: String },
    /// Create and activate a blank Texture Set at the active set's resolution.
    TextureSetCreate { set_id: u32, name: String },
    /// Remove the active Texture Set and activate the lowest-ID survivor.
    TextureSetRemove { set_id: u32 },
    /// Select a Texture Set by stable ID without changing its contents.
    TextureSetSelect { set_id: u32 },
    /// Resize the active Texture Set using the document's nearest-neighbor policy.
    TextureSetResize { set_id: u32, size: u32 },
    /// Replace bounded per-set ambient-occlusion bake parameters.
    TextureSetAoSettings {
        set_id: u32,
        sample_count: u32,
        max_ray_distance: f32,
        seed: u64,
    },
    /// Bind one imported-mesh material slot to a Texture Set, or use active-set fallback.
    /// Replay assumes the mesh matches the saved baseline; mesh changes are unsupported.
    MaterialBinding { slot: usize, set_id: Option<u32> },
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
    /// Remove a channel buffer that an undo operation originally created.
    LayerChannelRemove {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
    },
    /// Replace or create one channel with a uniform RGBA value.
    LayerImageFill {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
        value: [u8; 4],
    },
    /// Invert every byte of a layer mask. This operation is its own inverse.
    LayerMaskInvert { set_id: u32, layer_id: u32 },
    /// Set every byte of a layer mask to one value.
    LayerMaskSet {
        set_id: u32,
        layer_id: u32,
        value: u8,
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
    /// Set or clear a bounded procedural layer graph.
    LayerProceduralGraphSet {
        set_id: u32,
        layer_id: u32,
        graph: Option<ProceduralGraph>,
    },
    /// Move a layer to its final zero-based stack position.
    LayerReorder {
        set_id: u32,
        layer_id: u32,
        to_index: usize,
    },
    /// Select a layer by stable ID within one Texture Set.
    LayerSelection { set_id: u32, layer_id: u32 },
    /// Replace the common editable properties of any tree node.
    LayerTreeNodePropertiesSet {
        set_id: u32,
        node_id: u32,
        name: String,
        visible: bool,
        opacity: f32,
        blend: BlendMode,
        disabled_channels: HashSet<Channel>,
        procedural_graph: Option<ProceduralGraph>,
    },
    /// Set or remove a constant channel value on a Fill node.
    LayerTreeFillValueSet {
        set_id: u32,
        node_id: u32,
        channel: Channel,
        value: Option<[u8; 4]>,
    },
    /// Move a stable node ID after verifying its original parent and sibling index.
    LayerTreeNodeMove {
        set_id: u32,
        node_id: u32,
        from_parent_id: Option<u32>,
        from_index: u32,
        to_parent_id: Option<u32>,
        to_index: u32,
    },
    /// Remove a node after verifying its original location and choosing a
    /// surviving selection outside the removed subtree.
    LayerTreeNodeRemove {
        set_id: u32,
        node_id: u32,
        expected_parent_id: Option<u32>,
        expected_index: u32,
        selected_after_node_id: u32,
    },
    /// Begin an atomic insertion/restoration of an encoded recursive subtree.
    LayerTreeSubtreeBegin {
        transaction_id: Uuid,
        set_id: u32,
        root_node_id: u32,
        parent_id: Option<u32>,
        index: u32,
        selected_node_id: u32,
        node_count: u32,
        encoded_len: u64,
        sha256: [u8; 32],
    },
    /// Append a contiguous base64 chunk of the canonical subtree snapshot.
    LayerTreeSubtreeChunk {
        transaction_id: Uuid,
        offset: u64,
        data: String,
    },
    /// Validate and atomically attach the staged subtree.
    LayerTreeSubtreeCommit { transaction_id: Uuid },
    /// Begin an atomic imported-channel transaction. Events from the begin
    /// marker through its matching commit marker are discarded if the journal
    /// ends before the commit is durable.
    TextureImportBegin {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
    },
    /// Commit an atomic imported-channel transaction.
    TextureImportCommit {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
    },
    /// Begin an atomic generated channel-layer installation transaction.
    LayerChannelBegin {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
    },
    /// Commit an atomic generated channel-layer installation transaction.
    LayerChannelCommit {
        set_id: u32,
        layer_id: u32,
        channel: Channel,
    },
    /// Begin a complete editable-layer snapshot restore transaction.
    LayerSnapshotBegin {
        set_id: u32,
        layer_id: u32,
        to_index: usize,
        selected_after_id: u32,
        metadata: LayerSnapshotMetadata,
        channels: Vec<Channel>,
    },
    /// Write one contiguous, bounded slice of a snapshot image buffer.
    LayerSnapshotChunk {
        set_id: u32,
        layer_id: u32,
        target: LayerImageTarget,
        offset: u64,
        #[serde(with = "snapshot_bytes")]
        bytes: Vec<u8>,
    },
    /// Finish snapshot restoration and choose a layer by stable ID.
    LayerSnapshotCommit {
        set_id: u32,
        layer_id: u32,
        selected_after_id: u32,
    },
    /// Begin an atomic mesh replacement with normalized serialized mesh bytes.
    MeshReplaceBegin {
        transaction_id: Uuid,
        encoded_len: u64,
        sha256: [u8; 32],
        material_bindings: Vec<Option<u32>>,
    },
    /// Append one bounded contiguous slice of normalized mesh JSON bytes.
    MeshReplaceChunk {
        transaction_id: Uuid,
        offset: u64,
        #[serde(with = "mesh_chunk_bytes")]
        bytes: Vec<u8>,
    },
    /// Verify and atomically install a staged mesh replacement.
    MeshReplaceCommit { transaction_id: Uuid },
}

/// All forward-compatible editable properties needed to restore a layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerSnapshotMetadata {
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub blend: BlendMode,
    pub disabled_channels: Vec<Channel>,
    #[serde(default)]
    pub procedural_graph: Option<ProceduralGraph>,
    #[serde(default)]
    pub extensions: serde_json::Map<String, serde_json::Value>,
}

/// Snapshot chunks use compact base64 on disk while accepting the old JSON
/// byte-array representation for forward-compatible journal decoding.
mod snapshot_bytes {
    use super::MAX_SNAPSHOT_CHUNK_BYTES;
    use base64::{decode, encode};
    use serde::{de::Error, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct BytesVisitor;
        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("base64 snapshot bytes or a legacy byte array")
            }
            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                let max_encoded = MAX_SNAPSHOT_CHUNK_BYTES.div_ceil(3) * 4;
                if value.len() > max_encoded {
                    return Err(E::custom("snapshot chunk exceeds 512 KiB"));
                }
                let bytes = decode(value).map_err(E::custom)?;
                if bytes.len() > MAX_SNAPSHOT_CHUNK_BYTES {
                    return Err(E::custom("snapshot chunk exceeds 512 KiB"));
                }
                Ok(bytes)
            }
            fn visit_string<E: Error>(self, value: String) -> Result<Self::Value, E> {
                self.visit_str(&value)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut bytes = Vec::new();
                while let Some(byte) = sequence.next_element::<u8>()? {
                    if bytes.len() == MAX_SNAPSHOT_CHUNK_BYTES {
                        return Err(A::Error::custom("snapshot chunk exceeds 512 KiB"));
                    }
                    bytes.try_reserve(1).map_err(A::Error::custom)?;
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }
        deserializer.deserialize_any(BytesVisitor)
    }
}

const MAX_SNAPSHOT_CHUNK_BYTES: usize = 512 * 1024;
const MAX_MESH_REPLACE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MESH_REPLACE_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_LAYER_TREE_SUBTREE_BYTES: usize = 96 * 1024 * 1024;
const MAX_LAYER_TREE_SUBTREE_CHUNK_BYTES: usize = 256 * 1024;

mod mesh_chunk_bytes {
    use super::MAX_MESH_REPLACE_CHUNK_BYTES;
    use base64::{decode, encode};
    use serde::{de::Error, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct BytesVisitor;
        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("base64 mesh bytes or a legacy byte array")
            }
            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                let max_encoded = MAX_MESH_REPLACE_CHUNK_BYTES.div_ceil(3) * 4;
                if value.len() > max_encoded {
                    return Err(E::custom("mesh recovery chunk exceeds 4 MiB"));
                }
                let bytes = decode(value).map_err(E::custom)?;
                if bytes.len() > MAX_MESH_REPLACE_CHUNK_BYTES {
                    return Err(E::custom("mesh recovery chunk exceeds 4 MiB"));
                }
                Ok(bytes)
            }
            fn visit_string<E: Error>(self, value: String) -> Result<Self::Value, E> {
                self.visit_str(&value)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut bytes = Vec::new();
                while let Some(byte) = sequence.next_element::<u8>()? {
                    if bytes.len() == MAX_MESH_REPLACE_CHUNK_BYTES {
                        return Err(A::Error::custom("mesh recovery chunk exceeds 4 MiB"));
                    }
                    bytes.try_reserve(1).map_err(A::Error::custom)?;
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }
        deserializer.deserialize_any(BytesVisitor)
    }
}

/// Serialize an image buffer as base64 without cloning its pixels. Recovery
/// chunks apply a second base64 layer when transporting this canonical JSON.
struct TreeSnapshotBytes<'a>(&'a [u8]);

impl Serialize for TreeSnapshotBytes<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::encode(self.0))
    }
}

mod tree_snapshot_bytes {
    use super::MAX_LAYER_TREE_SUBTREE_BYTES;
    use base64::decode;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let max_encoded = MAX_LAYER_TREE_SUBTREE_BYTES.div_ceil(3) * 4;
        if encoded.len() > max_encoded {
            return Err(D::Error::custom("layer-tree image exceeds recovery bound"));
        }
        let bytes = decode(&encoded).map_err(D::Error::custom)?;
        if bytes.len() > MAX_LAYER_TREE_SUBTREE_BYTES {
            return Err(D::Error::custom("layer-tree image exceeds recovery bound"));
        }
        Ok(bytes)
    }
}

#[derive(Serialize)]
struct TreeSubtreeSnapshotRef<'a> {
    version: u32,
    nodes: Vec<TreeSubtreeNodeRef<'a>>,
}

#[derive(Serialize)]
struct TreeSubtreeNodeRef<'a> {
    parent_index: Option<usize>,
    sibling_index: usize,
    kind: TreeNodeSnapshotRef<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TreeNodeSnapshotRef<'a> {
    Paint {
        id: u32,
        name: &'a str,
        visible: bool,
        opacity: f32,
        blend: BlendMode,
        disabled_channels: Vec<Channel>,
        procedural_graph: &'a Option<ProceduralGraph>,
        extensions: &'a serde_json::Map<String, serde_json::Value>,
        mask: TreeSnapshotBytes<'a>,
        channels: Vec<TreeChannelSnapshotRef<'a>>,
    },
    Fill {
        properties: TreeNodePropertiesSnapshotRef<'a>,
        values: Vec<TreeFillValueSnapshotRef>,
    },
    Group {
        properties: TreeNodePropertiesSnapshotRef<'a>,
    },
}

#[derive(Serialize)]
struct TreeNodePropertiesSnapshotRef<'a> {
    id: u32,
    name: &'a str,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    disabled_channels: Vec<Channel>,
    extensions: &'a serde_json::Map<String, serde_json::Value>,
    mask: TreeSnapshotBytes<'a>,
}

#[derive(Serialize)]
struct TreeChannelSnapshotRef<'a> {
    channel: Channel,
    #[serde(serialize_with = "serialize_tree_snapshot_bytes")]
    bytes: &'a [u8],
}

fn serialize_tree_snapshot_bytes<S: serde::Serializer>(
    bytes: &&[u8],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&base64::encode(bytes))
}

#[derive(Deserialize)]
struct TreeSubtreeSnapshot {
    version: u32,
    #[serde(deserialize_with = "deserialize_bounded_tree_nodes")]
    nodes: Vec<TreeSubtreeNode>,
}

fn deserialize_bounded_tree_nodes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TreeSubtreeNode>, D::Error> {
    struct NodesVisitor;
    impl<'de> serde::de::Visitor<'de> for NodesVisitor {
        type Value = Vec<TreeSubtreeNode>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bounded layer-tree node array")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut nodes = Vec::new();
            nodes
                .try_reserve_exact(seq.size_hint().unwrap_or(0).min(MAX_LAYER_TREE_NODES))
                .map_err(serde::de::Error::custom)?;
            while let Some(node) = seq.next_element::<TreeSubtreeNode>()? {
                if nodes.len() == MAX_LAYER_TREE_NODES {
                    return Err(serde::de::Error::custom(
                        "layer-tree subtree exceeds the 4096-node limit",
                    ));
                }
                nodes.try_reserve(1).map_err(serde::de::Error::custom)?;
                nodes.push(node);
            }
            Ok(nodes)
        }
    }
    deserializer.deserialize_seq(NodesVisitor)
}

#[derive(Deserialize)]
struct TreeSubtreeNode {
    parent_index: Option<usize>,
    sibling_index: usize,
    kind: TreeNodeSnapshot,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TreeNodeSnapshot {
    Paint {
        id: u32,
        name: String,
        visible: bool,
        opacity: f32,
        blend: BlendMode,
        disabled_channels: Vec<Channel>,
        #[serde(default)]
        procedural_graph: Option<ProceduralGraph>,
        #[serde(default)]
        extensions: serde_json::Map<String, serde_json::Value>,
        #[serde(with = "tree_snapshot_bytes")]
        mask: Vec<u8>,
        #[serde(default)]
        channels: Vec<TreeChannelSnapshot>,
    },
    Fill {
        properties: TreeNodePropertiesSnapshot,
        values: Vec<TreeFillValueSnapshot>,
    },
    Group {
        properties: TreeNodePropertiesSnapshot,
    },
}

#[derive(Deserialize)]
struct TreeNodePropertiesSnapshot {
    id: u32,
    name: String,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    disabled_channels: Vec<Channel>,
    #[serde(default)]
    extensions: serde_json::Map<String, serde_json::Value>,
    #[serde(with = "tree_snapshot_bytes")]
    mask: Vec<u8>,
}

#[derive(Deserialize)]
struct TreeChannelSnapshot {
    channel: Channel,
    #[serde(with = "tree_snapshot_bytes")]
    bytes: Vec<u8>,
}

#[derive(Serialize)]
struct TreeFillValueSnapshotRef {
    channel: Channel,
    value: [u8; 4],
}

#[derive(Deserialize)]
struct TreeFillValueSnapshot {
    channel: Channel,
    value: [u8; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "channel", rename_all = "snake_case")]
pub enum LayerImageTarget {
    Channel(Channel),
    Mask,
}

/// Prepare a bounded restore transaction for a Paint, Fill, or Group subtree.
/// Snapshot serialization borrows node buffers and never clones the subtree.
pub fn prepare_layer_tree_subtree_events(
    node: &LayerNode,
    set_id: u32,
    parent_id: Option<u32>,
    index: u32,
    selected_node_id: u32,
) -> Result<Vec<DocumentEvent>> {
    if set_id == 0 || selected_node_id == 0 {
        bail!("Layer-tree recovery IDs must be nonzero");
    }
    let size = tree_node_size(node);
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Recovery subtree has an invalid texture dimension");
    }

    let mut stack = Vec::new();
    stack.try_reserve(1)?;
    stack.push((node, None, 0usize, 1usize));
    let mut nodes = Vec::new();
    nodes.try_reserve(1)?;
    let mut decoded_bytes = 0u64;
    let mut ids = HashSet::new();
    ids.try_reserve(1)?;
    while let Some((node, parent_index, sibling_index, depth)) = stack.pop() {
        if depth > MAX_LAYER_TREE_DEPTH {
            bail!("Recovery subtree exceeds the {MAX_LAYER_TREE_DEPTH}-level depth limit");
        }
        if nodes.len() >= MAX_LAYER_TREE_NODES {
            bail!("Recovery subtree exceeds the {MAX_LAYER_TREE_NODES}-node limit");
        }
        if node.id() == 0 || !ids.insert(node.id()) {
            bail!("Recovery subtree contains a zero or duplicate node ID");
        }
        let node_index = nodes.len();
        let kind = match node {
            LayerNode::Paint(layer) => {
                validate_tree_node_metadata(
                    layer.id,
                    &layer.name,
                    layer.opacity,
                    &layer.mask,
                    size,
                )?;
                if let Some(graph) = &layer.procedural_graph {
                    graph.validate()?;
                }
                if layer.pixels.len() > Channel::ALL.len() {
                    bail!("Recovery Paint node has too many channel images");
                }
                for image in layer.pixels.values() {
                    if image.dimensions() != (size, size) {
                        bail!("Recovery subtree Paint channel dimensions are invalid");
                    }
                    decoded_bytes = decoded_bytes
                        .checked_add(image.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Recovery subtree byte count overflow"))?;
                }
                decoded_bytes = decoded_bytes
                    .checked_add(layer.mask.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree byte count overflow"))?;
                let mut channels = Vec::new();
                channels.try_reserve_exact(layer.pixels.len())?;
                for channel in Channel::ALL {
                    if let Some(image) = layer.pixels.get(&channel) {
                        channels.push(TreeChannelSnapshotRef {
                            channel,
                            bytes: image.as_raw(),
                        });
                    }
                }
                TreeNodeSnapshotRef::Paint {
                    id: layer.id,
                    name: &layer.name,
                    visible: layer.visible,
                    opacity: layer.opacity,
                    blend: layer.blend,
                    disabled_channels: ordered_channels(&layer.disabled_channels),
                    procedural_graph: &layer.procedural_graph,
                    extensions: &layer.extensions,
                    mask: TreeSnapshotBytes(layer.mask.as_raw()),
                    channels,
                }
            }
            LayerNode::Fill(fill) => {
                let properties = &fill.properties;
                validate_tree_node_metadata(
                    properties.id,
                    &properties.name,
                    properties.opacity,
                    &properties.mask,
                    size,
                )?;
                if fill.values.is_empty()
                    || fill.values.len() > Channel::ALL.len()
                    || fill
                        .values
                        .keys()
                        .any(|channel| !Channel::ALL.contains(channel))
                {
                    bail!("Recovery Fill node has invalid channel values");
                }
                decoded_bytes = decoded_bytes
                    .checked_add(properties.mask.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree byte count overflow"))?;
                let values = Channel::ALL
                    .into_iter()
                    .filter_map(|channel| {
                        fill.values
                            .get(&channel)
                            .copied()
                            .map(|value| TreeFillValueSnapshotRef { channel, value })
                    })
                    .collect();
                TreeNodeSnapshotRef::Fill {
                    properties: properties_snapshot_ref(properties),
                    values,
                }
            }
            LayerNode::Group(group) => {
                let properties = &group.properties;
                validate_tree_node_metadata(
                    properties.id,
                    &properties.name,
                    properties.opacity,
                    &properties.mask,
                    size,
                )?;
                decoded_bytes = decoded_bytes
                    .checked_add(properties.mask.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree byte count overflow"))?;
                TreeNodeSnapshotRef::Group {
                    properties: properties_snapshot_ref(properties),
                }
            }
        };
        nodes.push(TreeSubtreeNodeRef {
            parent_index,
            sibling_index,
            kind,
        });
        if let LayerNode::Group(group) = node {
            stack.try_reserve(group.children.len())?;
            for (child_index, child) in group.children.iter().enumerate().rev() {
                stack.push((child, Some(node_index), child_index, depth + 1));
            }
        }
        if decoded_bytes > MAX_LAYER_TREE_SUBTREE_BYTES as u64 {
            bail!("Recovery subtree exceeds the 96-MiB retained payload limit");
        }
    }

    validate_tree_extensions_in_snapshot(&nodes)?;
    let snapshot = TreeSubtreeSnapshotRef { version: 1, nodes };
    let mut writer = BoundedWriter::new(MAX_LAYER_TREE_SUBTREE_BYTES);
    serde_json::to_writer(&mut writer, &snapshot)
        .map_err(|error| anyhow::anyhow!("Could not encode recovery subtree: {error}"))?;
    let bytes = writer.bytes;
    if bytes.is_empty() {
        bail!("Recovery subtree snapshot is empty");
    }
    let encoded_len = u64::try_from(bytes.len())
        .map_err(|_| anyhow::anyhow!("Recovery subtree length is not addressable"))?;
    let transaction_id = Uuid::new_v4();
    let mut events = Vec::new();
    let chunk_count = bytes.len().div_ceil(MAX_LAYER_TREE_SUBTREE_CHUNK_BYTES);
    events.try_reserve_exact(chunk_count.saturating_add(2))?;
    events.push(DocumentEvent::LayerTreeSubtreeBegin {
        transaction_id,
        set_id,
        root_node_id: node.id(),
        parent_id,
        index,
        selected_node_id,
        node_count: u32::try_from(ids.len())
            .map_err(|_| anyhow::anyhow!("Recovery subtree node count overflow"))?,
        encoded_len,
        sha256: Sha256::digest(&bytes).into(),
    });
    for (chunk_index, chunk) in bytes.chunks(MAX_LAYER_TREE_SUBTREE_CHUNK_BYTES).enumerate() {
        let offset = chunk_index
            .checked_mul(MAX_LAYER_TREE_SUBTREE_CHUNK_BYTES)
            .ok_or_else(|| anyhow::anyhow!("Recovery subtree chunk offset overflow"))?;
        events.push(DocumentEvent::LayerTreeSubtreeChunk {
            transaction_id,
            offset: offset as u64,
            data: base64::encode(chunk),
        });
    }
    events.push(DocumentEvent::LayerTreeSubtreeCommit { transaction_id });
    Ok(events)
}

fn tree_node_size(node: &LayerNode) -> u32 {
    match node {
        LayerNode::Paint(layer) => layer.mask.width(),
        LayerNode::Fill(fill) => fill.properties.mask.width(),
        LayerNode::Group(group) => group.properties.mask.width(),
    }
}

fn properties_snapshot_ref(properties: &LayerNodeProperties) -> TreeNodePropertiesSnapshotRef<'_> {
    TreeNodePropertiesSnapshotRef {
        id: properties.id,
        name: &properties.name,
        visible: properties.visible,
        opacity: properties.opacity,
        blend: properties.blend,
        disabled_channels: ordered_channels(&properties.disabled_channels),
        extensions: &properties.extensions,
        mask: TreeSnapshotBytes(properties.mask.as_raw()),
    }
}

fn ordered_channels(channels: &HashSet<Channel>) -> Vec<Channel> {
    Channel::ALL
        .into_iter()
        .filter(|channel| channels.contains(channel))
        .collect()
}

fn validate_tree_node_metadata(
    id: u32,
    name: &str,
    opacity: f32,
    mask: &GrayImage,
    size: u32,
) -> Result<()> {
    if id == 0 || name.is_empty() || name.len() > 1024 {
        bail!("Recovery subtree node ID or name is invalid");
    }
    if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
        bail!("Recovery subtree node opacity is invalid");
    }
    if mask.dimensions() != (size, size) {
        bail!("Recovery subtree node mask dimensions are invalid");
    }
    Ok(())
}

fn validate_tree_extensions_in_snapshot(nodes: &[TreeSubtreeNodeRef<'_>]) -> Result<()> {
    let mut total = 0usize;
    for node in nodes {
        let extensions = match &node.kind {
            TreeNodeSnapshotRef::Paint { extensions, .. } => *extensions,
            TreeNodeSnapshotRef::Fill { properties, .. }
            | TreeNodeSnapshotRef::Group { properties } => properties.extensions,
        };
        total = total
            .checked_add(extension_serialized_size(
                extensions,
                MAX_LAYER_EXTENSION_BYTES.saturating_sub(total),
            )?)
            .ok_or_else(|| anyhow::anyhow!("Recovery subtree extension size overflow"))?;
    }
    Ok(())
}

fn snapshot_node_id(snapshot: &TreeNodeSnapshot) -> u32 {
    match snapshot {
        TreeNodeSnapshot::Paint { id, .. } => *id,
        TreeNodeSnapshot::Fill { properties, .. } | TreeNodeSnapshot::Group { properties } => {
            properties.id
        }
    }
}

fn snapshot_node_is_group(snapshot: &TreeNodeSnapshot) -> bool {
    matches!(snapshot, TreeNodeSnapshot::Group { .. })
}

fn snapshot_node_extensions(
    snapshot: &TreeNodeSnapshot,
) -> &serde_json::Map<String, serde_json::Value> {
    match snapshot {
        TreeNodeSnapshot::Paint { extensions, .. } => extensions,
        TreeNodeSnapshot::Fill { properties, .. } | TreeNodeSnapshot::Group { properties } => {
            &properties.extensions
        }
    }
}

fn validate_tree_snapshot(
    snapshot: &TreeSubtreeSnapshot,
    expected_root_id: u32,
    expected_node_count: usize,
    size: u32,
) -> Result<(u64, usize)> {
    if snapshot.version != 1 || snapshot.nodes.is_empty() {
        bail!("Recovery subtree snapshot version or node list is invalid");
    }
    if snapshot.nodes.len() != expected_node_count || snapshot.nodes.len() > MAX_LAYER_TREE_NODES {
        bail!("Recovery subtree node count does not match its begin marker");
    }
    if snapshot.nodes[0].parent_index.is_some()
        || snapshot.nodes[0].sibling_index != 0
        || snapshot_node_id(&snapshot.nodes[0].kind) != expected_root_id
    {
        bail!("Recovery subtree root does not match its begin marker");
    }

    let expected_mask = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Recovery subtree mask size overflow"))?;
    let expected_image = expected_mask
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("Recovery subtree image size overflow"))?;
    let mut decoded = 0u64;
    let mut extension_bytes = 0usize;
    let mut ids = HashSet::new();
    ids.try_reserve(snapshot.nodes.len())?;
    let mut depths: Vec<usize> = Vec::new();
    depths.try_reserve_exact(snapshot.nodes.len())?;
    let mut child_counts = Vec::new();
    child_counts.try_reserve_exact(snapshot.nodes.len())?;
    child_counts.resize(snapshot.nodes.len(), 0usize);

    for (index, record) in snapshot.nodes.iter().enumerate() {
        let id = snapshot_node_id(&record.kind);
        if id == 0 || !ids.insert(id) {
            bail!("Recovery subtree contains a zero or duplicate node ID");
        }
        let depth = match record.parent_index {
            None if index == 0 => 1usize,
            None => bail!("Recovery subtree contains more than one root node"),
            Some(parent_index) => {
                if parent_index >= index
                    || !snapshot_node_is_group(&snapshot.nodes[parent_index].kind)
                {
                    bail!("Recovery subtree parent must be an earlier Group node");
                }
                let expected_sibling = child_counts[parent_index];
                if record.sibling_index != expected_sibling {
                    bail!("Recovery subtree child order is missing or duplicated");
                }
                child_counts[parent_index] = expected_sibling
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree child index overflow"))?;
                depths[parent_index]
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree depth overflow"))?
            }
        };
        if depth > MAX_LAYER_TREE_DEPTH {
            bail!("Recovery subtree exceeds the {MAX_LAYER_TREE_DEPTH}-level depth limit");
        }
        depths.push(depth);
        let (name, visible, opacity, blend, disabled, mask, channels, values, graph) =
            match &record.kind {
                TreeNodeSnapshot::Paint {
                    name,
                    visible,
                    opacity,
                    blend,
                    disabled_channels,
                    procedural_graph,
                    mask,
                    channels,
                    ..
                } => (
                    name,
                    *visible,
                    *opacity,
                    *blend,
                    disabled_channels.as_slice(),
                    mask.as_slice(),
                    channels.as_slice(),
                    None,
                    procedural_graph.as_ref(),
                ),
                TreeNodeSnapshot::Fill { properties, values } => (
                    &properties.name,
                    properties.visible,
                    properties.opacity,
                    properties.blend,
                    properties.disabled_channels.as_slice(),
                    properties.mask.as_slice(),
                    &[][..],
                    Some(values.as_slice()),
                    None,
                ),
                TreeNodeSnapshot::Group { properties } => (
                    &properties.name,
                    properties.visible,
                    properties.opacity,
                    properties.blend,
                    properties.disabled_channels.as_slice(),
                    properties.mask.as_slice(),
                    &[][..],
                    None,
                    None,
                ),
            };
        let _ = (visible, blend);
        if name.is_empty()
            || name.len() > 1024
            || !opacity.is_finite()
            || !(0.0..=1.0).contains(&opacity)
            || mask.len() as u64 != expected_mask
        {
            bail!("Recovery subtree node properties or mask are invalid");
        }
        let mut disabled_set = HashSet::new();
        disabled_set.try_reserve(disabled.len())?;
        for channel in disabled {
            if !Channel::ALL.contains(channel) || !disabled_set.insert(*channel) {
                bail!("Recovery subtree has invalid or duplicate disabled channels");
            }
        }
        decoded = decoded
            .checked_add(expected_mask)
            .ok_or_else(|| anyhow::anyhow!("Recovery subtree decoded byte count overflow"))?;
        let mut channel_set = HashSet::new();
        channel_set.try_reserve(channels.len())?;
        if channels.len() > Channel::ALL.len() {
            bail!("Recovery subtree Paint node has too many channels");
        }
        for image in channels {
            if !Channel::ALL.contains(&image.channel)
                || !channel_set.insert(image.channel)
                || image.bytes.len() as u64 != expected_image
            {
                bail!("Recovery subtree Paint channel metadata is invalid");
            }
            decoded = decoded
                .checked_add(expected_image)
                .ok_or_else(|| anyhow::anyhow!("Recovery subtree decoded byte count overflow"))?;
        }
        if let Some(values) = values {
            if values.is_empty() || values.len() > Channel::ALL.len() {
                bail!("Recovery subtree Fill node must define channel values");
            }
            channel_set.clear();
            for value in values {
                if !Channel::ALL.contains(&value.channel) || !channel_set.insert(value.channel) {
                    bail!("Recovery subtree Fill values have duplicate or invalid channels");
                }
            }
        }
        if let Some(graph) = graph {
            graph.validate()?;
        }
        extension_bytes = extension_bytes
            .checked_add(extension_serialized_size(
                snapshot_node_extensions(&record.kind),
                MAX_LAYER_EXTENSION_BYTES.saturating_sub(extension_bytes),
            )?)
            .ok_or_else(|| anyhow::anyhow!("Recovery subtree extension size overflow"))?;
        if decoded > MAX_LAYER_TREE_SUBTREE_BYTES as u64 {
            bail!("Recovery subtree exceeds the 96-MiB retained payload limit");
        }
    }
    Ok((decoded, extension_bytes))
}

fn snapshot_properties_into(
    snapshot: TreeNodePropertiesSnapshot,
    size: u32,
) -> Result<LayerNodeProperties> {
    let expected = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Recovery subtree mask size overflow"))?;
    if snapshot.mask.len() as u64 != expected {
        bail!("Recovery subtree mask size is invalid");
    }
    let mask = GrayImage::from_raw(size, size, snapshot.mask)
        .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery subtree mask"))?;
    let mut disabled_channels = HashSet::new();
    disabled_channels.try_reserve(snapshot.disabled_channels.len())?;
    disabled_channels.extend(snapshot.disabled_channels);
    Ok(LayerNodeProperties {
        id: snapshot.id,
        name: snapshot.name,
        visible: snapshot.visible,
        opacity: snapshot.opacity,
        blend: snapshot.blend,
        disabled_channels,
        mask,
        extensions: snapshot.extensions,
    })
}

fn snapshot_node_into(snapshot: TreeNodeSnapshot, size: u32) -> Result<LayerNode> {
    let expected_mask = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Recovery subtree mask size overflow"))?;
    let node = match snapshot {
        TreeNodeSnapshot::Paint {
            id,
            name,
            visible,
            opacity,
            blend,
            disabled_channels,
            procedural_graph,
            extensions,
            mask,
            channels,
        } => {
            if mask.len() as u64 != expected_mask {
                bail!("Recovery subtree Paint mask size is invalid");
            }
            let mask = GrayImage::from_raw(size, size, mask)
                .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery Paint mask"))?;
            let mut pixels = HashMap::new();
            pixels.try_reserve(channels.len())?;
            for image in channels {
                let mut seen = HashSet::new();
                if !Channel::ALL.contains(&image.channel) || pixels.contains_key(&image.channel) {
                    bail!("Recovery subtree Paint channels are invalid or duplicated");
                }
                seen.insert(image.channel);
                let expected = expected_mask
                    .checked_mul(4)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree image size overflow"))?;
                if image.bytes.len() as u64 != expected {
                    bail!("Recovery subtree Paint channel size is invalid");
                }
                let rgba = RgbaImage::from_raw(size, size, image.bytes)
                    .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery Paint channel"))?;
                pixels.insert(image.channel, rgba);
            }
            let mut disabled = HashSet::new();
            disabled.try_reserve(disabled_channels.len())?;
            disabled.extend(disabled_channels);
            LayerNode::Paint(Layer {
                id,
                name,
                visible,
                opacity,
                blend,
                pixels,
                disabled_channels: disabled,
                mask,
                procedural_graph,
                extensions,
            })
        }
        TreeNodeSnapshot::Fill { properties, values } => {
            let properties = snapshot_properties_into(properties, size)?;
            let mut fill_values = HashMap::new();
            fill_values.try_reserve(values.len())?;
            for item in values {
                if !Channel::ALL.contains(&item.channel)
                    || fill_values.insert(item.channel, item.value).is_some()
                {
                    bail!("Recovery subtree Fill values are invalid or duplicated");
                }
            }
            if fill_values.is_empty() {
                bail!("Recovery subtree Fill node has no channel values");
            }
            LayerNode::Fill(FillLayer {
                properties,
                values: fill_values,
            })
        }
        TreeNodeSnapshot::Group { properties } => LayerNode::Group(LayerGroup {
            properties: snapshot_properties_into(properties, size)?,
            children: Vec::new(),
        }),
    };
    Ok(node)
}

fn build_subtree(mut snapshot_nodes: Vec<TreeSubtreeNode>, size: u32) -> Result<LayerNode> {
    let parent_data = snapshot_nodes
        .iter()
        .map(|record| (record.parent_index, record.sibling_index))
        .collect::<Vec<_>>();
    let mut nodes = Vec::new();
    nodes.try_reserve_exact(snapshot_nodes.len())?;
    for record in snapshot_nodes.drain(..) {
        nodes.push(Some(snapshot_node_into(record.kind, size)?));
    }
    let node_count = nodes.len();
    // Parents precede their descendants in the validated preorder snapshot.
    // Build in reverse so every child is complete before its parent is moved
    // into the next ancestor. Siblings are then restored at the front because
    // their records are visited from the highest sibling index down.
    for index in (1..node_count).rev() {
        let (parent_index, _) = parent_data[index];
        let parent_index =
            parent_index.ok_or_else(|| anyhow::anyhow!("Recovery subtree has multiple roots"))?;
        let child = nodes[index]
            .take()
            .ok_or_else(|| anyhow::anyhow!("Recovery subtree node is duplicated"))?;
        let parent = nodes
            .get_mut(parent_index)
            .and_then(Option::as_mut)
            .ok_or_else(|| anyhow::anyhow!("Recovery subtree parent order is invalid"))?;
        let LayerNode::Group(group) = parent else {
            bail!("Recovery subtree child parent is not a Group");
        };
        group.children.try_reserve(1)?;
        group.children.insert(0, child);
    }
    nodes[0]
        .take()
        .ok_or_else(|| anyhow::anyhow!("Recovery subtree root is missing"))
}

impl DocumentEvent {
    /// Encode one event without allowing JSON expansion to exceed the record cap.
    pub fn encode_bounded(&self) -> io::Result<Vec<u8>> {
        if matches!(self, Self::LayerSnapshotChunk { bytes, .. } if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_CHUNK_BYTES)
        {
            return Err(invalid_input("snapshot chunk must contain 1–512 KiB"));
        }
        if matches!(self, Self::MeshReplaceChunk { bytes, .. } if bytes.is_empty() || bytes.len() > MAX_MESH_REPLACE_CHUNK_BYTES)
        {
            return Err(invalid_input("mesh recovery chunk must contain 1–4 MiB"));
        }
        validate_tree_subtree_event_bounds(self)
            .map_err(|error| invalid_data_owned(error.to_string()))?;
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

    /// Apply one deterministic mutation to a candidate document. All IDs,
    /// dimensions, target buffers and projected decoded bytes are checked first.
    pub fn apply(&self, doc: &mut Document) -> Result<()> {
        match self {
            Self::TextureSetCreate { set_id, name } => {
                if *set_id == 0 || *set_id != doc.next_set_id {
                    bail!("Recovery Texture Set ID does not match the next set ID");
                }
                let trimmed = name.trim();
                if trimmed.is_empty() || trimmed.len() > 128 {
                    bail!("Recovery Texture Set name must contain 1–128 bytes");
                }
                if doc.texture_set_count() >= MAX_TEXTURE_SETS {
                    bail!("Recovery exceeds the {MAX_TEXTURE_SETS}-set limit");
                }
                if doc.active_set_id == 0 || doc.active_set_id >= *set_id {
                    bail!("Recovery Texture Set ID is not greater than existing IDs");
                }
                let mut existing_ids = HashSet::new();
                existing_ids.try_reserve(doc.texture_sets.len().saturating_add(1))?;
                existing_ids.insert(doc.active_set_id);
                for set in &doc.texture_sets {
                    if set.id == 0 || set.id >= *set_id || !existing_ids.insert(set.id) {
                        bail!("Recovery Texture Set IDs are invalid or ambiguous");
                    }
                }
                let following_set_id = set_id
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Recovery Texture Set ID counter exhausted"))?;
                let size = doc.size;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid active Texture Set resolution");
                }
                let mask_bytes = u64::from(size)
                    .checked_mul(u64::from(size))
                    .ok_or_else(|| anyhow::anyhow!("Recovery Texture Set mask size overflow"))?;
                if !layer_allocation_fits(doc.decoded_bytes(), mask_bytes) {
                    bail!("Recovery Texture Set exceeds the 512-MiB decoded texture budget");
                }

                let normalized_name = fallible_string(trimmed)?;
                let new_layer = blank_layer_fallible(1, "Paint Layer", size)?;
                let mut roots = Vec::new();
                roots.try_reserve_exact(1)?;
                roots.push(LayerNode::Paint(new_layer));
                let new_set = TextureSet {
                    id: *set_id,
                    name: normalized_name,
                    size,
                    layer_tree: LayerTree {
                        roots,
                        selected_node_id: 1,
                        next_node_id: 2,
                        extensions: Default::default(),
                    },
                    extensions: Default::default(),
                };

                // Reserve before moving any current active-set fields.
                doc.texture_sets.try_reserve(1)?;
                let previous = TextureSet {
                    id: doc.active_set_id,
                    name: std::mem::take(&mut doc.active_set_name),
                    size: doc.size,
                    layer_tree: std::mem::replace(
                        &mut doc.layer_tree,
                        LayerTree {
                            roots: Vec::new(),
                            selected_node_id: 0,
                            next_node_id: 1,
                            extensions: Default::default(),
                        },
                    ),
                    extensions: std::mem::take(&mut doc.active_set_extensions),
                };
                doc.texture_sets.push(previous);
                doc.active_set_id = new_set.id;
                doc.active_set_name = new_set.name;
                doc.size = new_set.size;
                doc.layer_tree = new_set.layer_tree;
                doc.active_set_extensions = new_set.extensions;
                doc.next_set_id = following_set_id;
                return Ok(());
            }
            Self::TextureSetRemove { set_id } => {
                if *set_id == 0 || *set_id != doc.active_set_id {
                    bail!("Recovery can remove only the active Texture Set");
                }
                if doc.texture_sets.is_empty() {
                    bail!("Recovery cannot remove the final Texture Set");
                }
                if doc.active_set_id == 0
                    || doc.next_set_id <= doc.active_set_id
                    || doc.texture_sets.iter().any(|set| {
                        set.id == 0 || set.id >= doc.next_set_id || set.id == doc.active_set_id
                    })
                {
                    bail!("Recovery Texture Set IDs are invalid or ambiguous");
                }
                if !(64..=MAX_SIZE).contains(&doc.size)
                    || doc.layer_tree.validate(doc.size).is_err()
                {
                    bail!("Recovery active Texture Set is invalid");
                }
                let mut ids = HashSet::new();
                ids.try_reserve(doc.texture_sets.len())?;
                let mut selected_id = None;
                for set in &doc.texture_sets {
                    if !ids.insert(set.id) {
                        bail!("Recovery Texture Set IDs are ambiguous");
                    }
                    if selected_id.is_none_or(|current| set.id < current) {
                        selected_id = Some(set.id);
                    }
                }
                let survivor_id =
                    selected_id.ok_or_else(|| anyhow::anyhow!("No surviving Texture Set"))?;
                let survivor_index = doc
                    .texture_sets
                    .iter()
                    .position(|set| set.id == survivor_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {survivor_id}"))?;
                let survivor = &doc.texture_sets[survivor_index];
                if !(64..=MAX_SIZE).contains(&survivor.size)
                    || survivor.layer_tree.validate(survivor.size).is_err()
                {
                    bail!("Recovery survivor Texture Set is invalid");
                }

                // The survivor and all IDs are validated. The remaining
                // ownership moves and removals are allocation-free.
                let target = doc.texture_sets.swap_remove(survivor_index);
                let previous = TextureSet {
                    id: doc.active_set_id,
                    name: std::mem::take(&mut doc.active_set_name),
                    size: doc.size,
                    layer_tree: std::mem::replace(
                        &mut doc.layer_tree,
                        LayerTree {
                            roots: Vec::new(),
                            selected_node_id: 0,
                            next_node_id: 1,
                            extensions: Default::default(),
                        },
                    ),
                    extensions: std::mem::take(&mut doc.active_set_extensions),
                };
                doc.texture_sets.push(previous);
                let removed_index = doc
                    .texture_sets
                    .iter()
                    .position(|set| set.id == *set_id)
                    .expect("the removed active set was just moved into the inactive list");
                drop(doc.texture_sets.swap_remove(removed_index));

                doc.active_set_id = target.id;
                doc.active_set_name = target.name;
                doc.size = target.size;
                doc.layer_tree = target.layer_tree;
                doc.active_set_extensions = target.extensions;
                for binding in &mut doc.material_bindings {
                    if *binding == Some(*set_id) {
                        *binding = None;
                    }
                }
                return Ok(());
            }
            Self::TextureSetSelect { set_id } => {
                if *set_id == 0 {
                    bail!("Recovery Texture Set ID must be nonzero");
                }
                if doc.texture_set_count() > MAX_TEXTURE_SETS
                    || doc.active_set_id == 0
                    || doc.next_set_id <= doc.active_set_id
                {
                    bail!("Recovery Texture Set IDs are invalid");
                }
                let mut ids = HashSet::new();
                ids.try_reserve(doc.texture_sets.len().saturating_add(1))?;
                ids.insert(doc.active_set_id);
                for set in &doc.texture_sets {
                    if set.id == 0 || set.id >= doc.next_set_id || !ids.insert(set.id) {
                        bail!("Recovery Texture Set IDs are invalid or ambiguous");
                    }
                }
                if !ids.contains(set_id) {
                    bail!("Unknown recovery Texture Set {set_id}");
                }
                if *set_id == doc.active_set_id {
                    return Ok(());
                }

                // All stable IDs have been validated. The existing switch
                // transfers ownership without cloning and has no fallible
                // work after its target lookup.
                doc.activate_texture_set(*set_id)?;
                return Ok(());
            }
            Self::TextureSetResize { set_id, size } => {
                if *set_id == 0 || *set_id != doc.active_set_id {
                    bail!("Recovery can resize only the active Texture Set");
                }
                if !(64..=MAX_SIZE).contains(size) {
                    bail!("Recovery Texture Set resolution is outside the supported range");
                }
                doc.try_resize(*size)?;
                return Ok(());
            }
            Self::TextureSetAoSettings {
                set_id,
                sample_count,
                max_ray_distance,
                seed,
            } => {
                let set_size = if *set_id == doc.active_set_id {
                    doc.size
                } else {
                    doc.texture_sets
                        .iter()
                        .find(|set| set.id == *set_id)
                        .map(|set| set.size)
                        .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?
                };
                let settings = crate::bake::AmbientOcclusionBakeSettings {
                    sample_count: *sample_count,
                    max_ray_distance: *max_ray_distance,
                    seed: *seed,
                };
                settings.validate_for_size(set_size)?;
                let mut value = serde_json::to_value(settings)?;
                let extensions = if *set_id == doc.active_set_id {
                    &mut doc.active_set_extensions
                } else {
                    &mut doc
                        .texture_sets
                        .iter_mut()
                        .find(|set| set.id == *set_id)
                        .expect("the Texture Set was validated above")
                        .extensions
                };
                if let (Some(existing), Some(replacement)) = (
                    extensions
                        .get(crate::bake::AO_SETTINGS_EXTENSION_KEY)
                        .and_then(serde_json::Value::as_object),
                    value.as_object_mut(),
                ) {
                    for (key, unknown) in existing {
                        if !["sample_count", "max_ray_distance", "seed"].contains(&key.as_str()) {
                            replacement
                                .entry(key.clone())
                                .or_insert_with(|| unknown.clone());
                        }
                    }
                }
                extensions.insert(crate::bake::AO_SETTINGS_EXTENSION_KEY.to_owned(), value);
                return Ok(());
            }
            Self::MaterialBinding { slot, set_id } => {
                let material_count = doc.mesh.material_count();
                if *slot >= material_count {
                    bail!("Recovery material slot is out of range");
                }
                if let Some(set_id) = set_id {
                    if *set_id == 0 || *set_id >= doc.next_set_id {
                        bail!("Recovery Texture Set ID is invalid");
                    }
                    let known_matches = usize::from(doc.active_set_id == *set_id)
                        + doc
                            .texture_sets
                            .iter()
                            .filter(|set| set.id == *set_id)
                            .count();
                    if known_matches != 1 {
                        bail!("Unknown or ambiguous recovery Texture Set {set_id}");
                    }
                }

                // `bind_material` historically extends short binding arrays.
                // Reserve that full extension before changing any slot; a
                // normal, exact-length vector keeps its length unchanged.
                if doc.material_bindings.len() < material_count {
                    let additional = material_count - doc.material_bindings.len();
                    doc.material_bindings.try_reserve(additional)?;
                    doc.material_bindings.resize(material_count, None);
                }
                doc.material_bindings[*slot] = *set_id;
                return Ok(());
            }
            Self::TextureSetRename { set_id, name } => {
                if *set_id == 0 {
                    bail!("Recovery Texture Set ID must be nonzero");
                }
                let trimmed = name.trim();
                if trimmed.is_empty() || trimmed.len() > 128 {
                    bail!("Recovery Texture Set name must contain 1–128 bytes");
                }
                if *set_id != doc.active_set_id
                    && !doc.texture_sets.iter().any(|set| set.id == *set_id)
                {
                    bail!("Unknown recovery Texture Set {set_id}");
                }

                let mut normalized_name = String::new();
                normalized_name.try_reserve_exact(trimmed.len())?;
                normalized_name.push_str(trimmed);
                *texture_set_name_mut(doc, *set_id)? = normalized_name;
                return Ok(());
            }
            Self::LayerDuplicate {
                set_id,
                source_layer_id,
                layer_id,
                to_index,
            } => {
                if *set_id == 0 {
                    bail!("Recovery Texture Set ID must be nonzero");
                }
                if *source_layer_id == 0 || *layer_id == 0 {
                    bail!("Recovery layer IDs must be nonzero");
                }

                let decoded_bytes_before = doc.decoded_bytes();
                let existing_extension_bytes = layer_extension_bytes(doc)?;
                let (tree, size) = layer_tree_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery Texture Set resolution");
                }
                let selected_id = tree.selected_node_id;
                let source_index = root_paint_index(tree, *source_layer_id)?;
                if source_index >= tree.roots.len() {
                    bail!("Recovery source Paint node index is invalid");
                }
                let expected_index = source_index
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Recovery layer index overflow"))?;
                if *to_index != expected_index || *to_index > tree.roots.len() {
                    bail!("Recovery duplicate insertion index is invalid");
                }
                if tree.roots.len() >= MAX_LAYERS {
                    bail!("Recovery layer count exceeds the {MAX_LAYERS}-layer limit");
                }
                if selected_id != *source_layer_id {
                    bail!("Recovery can duplicate only the selected layer");
                }
                if *layer_id != tree.next_node_id {
                    bail!("Recovery duplicate ID does not match the next layer ID");
                }
                let following_layer_id = layer_id
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Recovery layer ID counter exhausted"))?;
                if tree.find_node(*layer_id).is_some() {
                    bail!("Recovery duplicate ID already exists");
                }

                let source = root_paint_layer(tree, *source_layer_id)?;
                let extra_bytes = layer_storage_bytes_checked(source, size)?;
                if !layer_allocation_fits(decoded_bytes_before, extra_bytes) {
                    bail!("Recovery duplicate exceeds the 512-MiB decoded texture budget");
                }
                let source_extension_bytes =
                    extension_serialized_size(&source.extensions, MAX_LAYER_EXTENSION_BYTES)?;
                if existing_extension_bytes
                    .checked_add(source_extension_bytes)
                    .is_none_or(|bytes| bytes > MAX_LAYER_EXTENSION_BYTES)
                {
                    bail!("Recovery duplicate exceeds the 48-MiB aggregate layer extension limit");
                }
                let duplicate = duplicate_layer_fallible(source, *layer_id, size)?;

                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                tree.insert(None, *to_index, LayerNode::Paint(duplicate))?;
                tree.select(*layer_id)?;
                tree.next_node_id = following_layer_id;
                return Ok(());
            }
            Self::LayerReinsert {
                set_id,
                layer_id,
                to_index,
                name,
                source_layer_id,
            } => {
                if *set_id == 0 || *layer_id == 0 {
                    bail!("Recovery layer IDs must be nonzero");
                }
                if name.is_empty() || name.len() > 1024 {
                    bail!("Recovery layer name must contain 1–1024 bytes");
                }
                let decoded_bytes_before = doc.decoded_bytes();
                let existing_extension_bytes = layer_extension_bytes(doc)?;
                let (tree, size) = layer_tree_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery Texture Set resolution");
                }
                if tree.roots.len() >= MAX_LAYERS {
                    bail!("Recovery layer count exceeds the {MAX_LAYERS}-layer limit");
                }
                if *layer_id >= tree.next_node_id {
                    bail!("Recovery reinsert ID was not previously allocated");
                }
                if tree.find_node(*layer_id).is_some() {
                    bail!("Recovery reinsert ID already exists");
                }
                let selected_index = root_paint_index(tree, tree.selected_node_id)?;
                if *to_index > tree.roots.len() {
                    bail!("Recovery layer reinsert index is out of range");
                }
                let selected_id = tree.selected_node_id;

                let reinserted = if let Some(source_id) = source_layer_id {
                    if *source_id == 0 {
                        bail!("Recovery source layer ID must be nonzero");
                    }
                    let source_index = root_paint_index(tree, *source_id)?;
                    if *source_id != selected_id
                        || source_index != selected_index
                        || *to_index != source_index + 1
                    {
                        bail!("Recovery duplicate reinsert must follow the selected source");
                    }
                    let source = root_paint_layer(tree, *source_id)?;
                    let extra_bytes = layer_storage_bytes_checked(source, size)?;
                    if !layer_allocation_fits(decoded_bytes_before, extra_bytes) {
                        bail!("Recovery duplicate reinsert exceeds the 512-MiB decoded texture budget");
                    }
                    let source_extension_bytes =
                        extension_serialized_size(&source.extensions, MAX_LAYER_EXTENSION_BYTES)?;
                    if existing_extension_bytes
                        .checked_add(source_extension_bytes)
                        .is_none_or(|bytes| bytes > MAX_LAYER_EXTENSION_BYTES)
                    {
                        bail!("Recovery duplicate reinsert exceeds the 48-MiB aggregate layer extension limit");
                    }
                    let mut duplicate = duplicate_layer_fallible(source, *layer_id, size)?;
                    duplicate.name = fallible_string(name)?;
                    duplicate
                } else {
                    if *to_index != selected_index.saturating_add(1) {
                        bail!("Recovery blank layer reinsert position is invalid");
                    }
                    let mask_bytes = u64::from(size)
                        .checked_mul(u64::from(size))
                        .ok_or_else(|| anyhow::anyhow!("Recovery layer mask size overflow"))?;
                    if !layer_allocation_fits(decoded_bytes_before, mask_bytes) {
                        bail!("Recovery layer reinsert exceeds the 512-MiB decoded texture budget");
                    }
                    blank_layer_fallible(*layer_id, name, size)?
                };

                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                tree.insert(None, *to_index, LayerNode::Paint(reinserted))?;
                tree.select(selected_id)?;
                return Ok(());
            }
            Self::LayerRemove {
                set_id,
                layer_id,
                selected_after_id,
            } => {
                if *set_id == 0 {
                    bail!("Recovery Texture Set ID must be nonzero");
                }
                if *layer_id == 0 || *selected_after_id == 0 {
                    bail!("Recovery layer IDs must be nonzero");
                }

                let (tree, size) = layer_tree_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery Texture Set resolution");
                }
                if tree.roots.len() <= 1 {
                    bail!("Recovery cannot remove the final layer");
                }
                let target_index = root_paint_index(tree, *layer_id)?;
                if tree.selected_node_id != *layer_id {
                    bail!("Recovery can remove only the selected layer");
                }
                let selected_after_index = root_paint_index(tree, *selected_after_id)?;
                if selected_after_index == target_index {
                    bail!("Recovery replacement selection must be another layer");
                }

                // All checks are complete. Removal cannot allocate, and the
                // replacement selection was proven unique before mutation. The
                // discarded layer's stored content needs no replay payload.
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                tree.remove(*layer_id, *selected_after_id)?;
                return Ok(());
            }
            Self::LayerCreate {
                set_id,
                layer_id,
                to_index,
                name,
            } => {
                if *set_id == 0 {
                    bail!("Recovery Texture Set ID must be nonzero");
                }
                if *layer_id == 0 {
                    bail!("Recovery layer ID must be nonzero");
                }
                if name.is_empty() || name.len() > 1024 {
                    bail!("Recovery layer name must contain 1–1024 bytes");
                }

                let decoded_bytes_before = doc.decoded_bytes();
                let (tree, size) = layer_tree_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery Texture Set resolution");
                }
                if tree.roots.len() >= MAX_LAYERS {
                    bail!("Recovery layer count exceeds the {MAX_LAYERS}-layer limit");
                }
                if *to_index > tree.roots.len() {
                    bail!("Recovery layer insertion index is out of range");
                }
                if *layer_id != tree.next_node_id {
                    bail!("Recovery layer ID does not match the next layer ID");
                }
                let following_layer_id = layer_id
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Recovery layer ID counter exhausted"))?;
                if tree.find_node(*layer_id).is_some() {
                    bail!("Recovery layer ID already exists");
                }
                let selected_id = tree.selected_node_id;
                if tree.find_node(selected_id).is_none() {
                    bail!("Recovery selected node ID is invalid");
                }

                let mask_bytes = u64::from(size)
                    .checked_mul(u64::from(size))
                    .ok_or_else(|| anyhow::anyhow!("Recovery layer mask size overflow"))?;
                if !layer_allocation_fits(decoded_bytes_before, mask_bytes) {
                    bail!("Recovery layer exceeds the 512-MiB decoded texture budget");
                }
                let new_layer = blank_layer_fallible(*layer_id, name, size)?;

                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                tree.insert(None, *to_index, LayerNode::Paint(new_layer))?;
                tree.select(selected_id)?;
                tree.next_node_id = following_layer_id;
                return Ok(());
            }
            Self::LayerRename {
                set_id,
                layer_id,
                name,
            } => {
                if name.is_empty() || name.len() > 1024 {
                    bail!("Recovery layer name must contain 1–1024 bytes");
                }
                let layer = root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?;
                layer.name.clone_from(name);
                return Ok(());
            }
            Self::LayerVisibility {
                set_id,
                layer_id,
                visible,
            } => {
                root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?.visible =
                    *visible;
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
                root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?.opacity =
                    *opacity;
                return Ok(());
            }
            Self::LayerChannelEnabled {
                set_id,
                layer_id,
                channel,
                enabled,
            } => {
                let layer = root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?;
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
                root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?.blend =
                    *blend;
                return Ok(());
            }
            Self::LayerProceduralGraphSet {
                set_id,
                layer_id,
                graph,
            } => {
                if let Some(graph) = graph {
                    graph.validate()?;
                }
                root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?
                    .procedural_graph = graph.clone();
                return Ok(());
            }
            Self::LayerReorder {
                set_id,
                layer_id,
                to_index,
            } => {
                let (tree, _) = layer_tree_for_set(doc, *set_id)?;
                root_paint_index(tree, *layer_id)?;
                if *to_index >= tree.roots.len() {
                    bail!("Recovery layer destination index is out of range");
                }
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                tree.move_node(*layer_id, None, *to_index)?;
                return Ok(());
            }
            Self::LayerSelection { set_id, layer_id } => {
                let tree = layer_tree_for_set(doc, *set_id)?.0;
                if tree.find_node(*layer_id).is_none() {
                    bail!("Unknown recovery layer tree selection ID {layer_id}");
                }
                layer_tree_for_set_mut(doc, *set_id)?.select(*layer_id)?;
                return Ok(());
            }
            Self::LayerChannelRemove {
                set_id,
                layer_id,
                channel,
            } => {
                root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?
                    .pixels
                    .remove(channel)
                    .ok_or_else(|| anyhow::anyhow!("Recovery channel to remove does not exist"))?;
                return Ok(());
            }
            Self::LayerImageFill {
                set_id,
                layer_id,
                channel,
                value,
            } => {
                if *set_id == 0 || *layer_id == 0 {
                    bail!("Recovery image fill IDs must be nonzero");
                }
                let decoded_before = doc.decoded_bytes();
                let size = layer_tree_size_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery image fill Texture Set resolution");
                }
                let area = u64::from(size)
                    .checked_mul(u64::from(size))
                    .ok_or_else(|| anyhow::anyhow!("Recovery image fill size overflow"))?;
                let full_bytes = area
                    .checked_mul(4)
                    .ok_or_else(|| anyhow::anyhow!("Recovery image fill byte size overflow"))?;
                let tree = layer_tree_for_set(doc, *set_id)?.0;
                let layer = root_paint_layer(tree, *layer_id)?;
                if let Some(image) = layer.pixels.get(channel) {
                    let expected_len = usize::try_from(full_bytes).map_err(|_| {
                        anyhow::anyhow!("Recovery image fill size is not addressable")
                    })?;
                    if image.dimensions() != (size, size) || image.as_raw().len() != expected_len {
                        bail!("Recovery image fill target dimensions do not match its Texture Set");
                    }
                    let layer =
                        root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?;
                    let pixels = layer.pixels.get_mut(channel).expect("validated channel");
                    for pixel in pixels.as_mut().chunks_exact_mut(4) {
                        pixel.copy_from_slice(value);
                    }
                } else {
                    if !channel_allocation_fits(decoded_before, full_bytes) {
                        bail!("Recovery image fill exceeds the 512-MiB decoded texture budget");
                    }
                    let allocation = usize::try_from(full_bytes).map_err(|_| {
                        anyhow::anyhow!("Recovery image fill size is not addressable")
                    })?;
                    let mut bytes = Vec::new();
                    bytes.try_reserve_exact(allocation)?;
                    bytes.resize(allocation, 0);
                    for pixel in bytes.chunks_exact_mut(4) {
                        pixel.copy_from_slice(value);
                    }
                    let image = RgbaImage::from_raw(size, size, bytes).ok_or_else(|| {
                        anyhow::anyhow!("Could not allocate recovery image fill channel")
                    })?;
                    let layer =
                        root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?;
                    layer.pixels.try_reserve(1)?;
                    layer.pixels.insert(*channel, image);
                }
                return Ok(());
            }
            Self::LayerMaskInvert { set_id, layer_id } => {
                if *set_id == 0 || *layer_id == 0 {
                    bail!("Recovery mask inversion IDs must be nonzero");
                }
                let size = layer_tree_size_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery mask Texture Set resolution");
                }
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                let node = tree
                    .find_node_mut(*layer_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown recovery node {layer_id}"))?;
                let mask = node_mask_mut(node);
                let expected_len = (size as usize)
                    .checked_mul(size as usize)
                    .ok_or_else(|| anyhow::anyhow!("Recovery mask size overflow"))?;
                if mask.dimensions() != (size, size) || mask.as_raw().len() != expected_len {
                    bail!("Recovery target mask dimensions do not match its Texture Set");
                }
                for value in mask.as_mut() {
                    *value = !*value;
                }
                return Ok(());
            }
            Self::LayerMaskSet {
                set_id,
                layer_id,
                value,
            } => {
                if *set_id == 0 || *layer_id == 0 {
                    bail!("Recovery mask fill IDs must be nonzero");
                }
                let size = layer_tree_size_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery mask Texture Set resolution");
                }
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                let node = tree
                    .find_node_mut(*layer_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown recovery node {layer_id}"))?;
                let mask = node_mask_mut(node);
                let expected_len = (size as usize)
                    .checked_mul(size as usize)
                    .ok_or_else(|| anyhow::anyhow!("Recovery mask size overflow"))?;
                if mask.dimensions() != (size, size) || mask.as_raw().len() != expected_len {
                    bail!("Recovery target mask dimensions do not match its Texture Set");
                }
                mask.as_mut().fill(*value);
                return Ok(());
            }
            Self::LayerSnapshotBegin {
                set_id,
                layer_id,
                to_index,
                selected_after_id,
                metadata,
                channels,
            } => {
                if *set_id == 0 || *layer_id == 0 || *selected_after_id == 0 {
                    bail!("Recovery snapshot IDs must be nonzero");
                }
                if metadata.name.is_empty() || metadata.name.len() > 1024 {
                    bail!("Recovery snapshot layer name must contain 1–1024 bytes");
                }
                if !metadata.opacity.is_finite() || !(0.0..=1.0).contains(&metadata.opacity) {
                    bail!("Recovery snapshot opacity must be finite and within 0..=1");
                }
                validate_snapshot_channels(channels)?;
                validate_snapshot_disabled_channels(&metadata.disabled_channels)?;
                if let Some(graph) = &metadata.procedural_graph {
                    graph.validate()?;
                }
                let extension_size =
                    extension_serialized_size(&metadata.extensions, MAX_LAYER_EXTENSION_BYTES)?;
                let existing_extensions = layer_extension_bytes(doc)?;
                if existing_extensions
                    .checked_add(extension_size)
                    .is_none_or(|bytes| bytes > MAX_LAYER_EXTENSION_BYTES)
                {
                    bail!("Recovery snapshot exceeds the 48-MiB aggregate layer extension limit");
                }

                let decoded_before = doc.decoded_bytes();
                let (tree, size) = layer_tree_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery Texture Set resolution");
                }
                let existing_index = tree.roots.iter().position(|node| node.id() == *layer_id);
                if existing_index
                    .is_some_and(|index| !matches!(tree.roots[index], LayerNode::Paint(_)))
                {
                    bail!("Recovery legacy snapshot target is not a root Paint node");
                }
                if existing_index.is_none() && tree.roots.len() >= MAX_LAYERS {
                    bail!("Recovery layer count exceeds the {MAX_LAYERS}-layer limit");
                }
                if *to_index > tree.roots.len() {
                    bail!("Recovery snapshot insertion or selection index is out of range");
                }
                if *layer_id >= tree.next_node_id
                    || existing_index.is_some_and(|index| index != *to_index)
                {
                    bail!(
                        "Recovery snapshot ID is not allocated or its replacement index is invalid"
                    );
                }
                let selected_exists =
                    *selected_after_id == *layer_id || tree.find_node(*selected_after_id).is_some();
                if !selected_exists {
                    bail!("Recovery snapshot final selection ID is unknown or ambiguous");
                }
                let area = u64::from(size)
                    .checked_mul(u64::from(size))
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot image size overflow"))?;
                let full_bytes = area
                    .checked_mul(1 + 4 * channels.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot storage size overflow"))?;
                let replaced_bytes = existing_index.map_or(0u64, |index| {
                    let LayerNode::Paint(layer) = &tree.roots[index] else {
                        return 0;
                    };
                    layer.mask.as_raw().len().saturating_add(
                        layer
                            .pixels
                            .values()
                            .map(|image| image.as_raw().len())
                            .fold(0usize, usize::saturating_add),
                    ) as u64
                });
                let decoded_after = decoded_before
                    .checked_sub(replaced_bytes)
                    .and_then(|bytes| bytes.checked_add(full_bytes))
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot storage size overflow"))?;
                if decoded_after > MAX_DECODED_BYTES {
                    bail!("Recovery snapshot exceeds the 512-MiB decoded texture budget");
                }

                // Drop the replaced layer before allocating its blank recovery
                // shell so peak decoded memory stays within the same budget.
                let mut layer = blank_layer_fallible(*layer_id, &metadata.name, size)?;
                layer.visible = metadata.visible;
                layer.opacity = metadata.opacity;
                layer.blend = metadata.blend;
                layer.procedural_graph = metadata.procedural_graph.clone();
                layer
                    .disabled_channels
                    .try_reserve(metadata.disabled_channels.len())?;
                layer
                    .disabled_channels
                    .extend(metadata.disabled_channels.iter().copied());
                // JSON maps cannot reserve fallibly; serialize and cap it before
                // cloning so malformed or enormous extension data fails first.
                layer.extensions = metadata.extensions.clone();
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                if let Some(index) = existing_index {
                    tree.roots[index] = LayerNode::Paint(layer);
                } else {
                    tree.insert(None, *to_index, LayerNode::Paint(layer))?;
                }
                return Ok(());
            }
            Self::LayerSnapshotChunk {
                set_id,
                layer_id,
                target,
                offset,
                bytes,
            } => {
                if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_CHUNK_BYTES {
                    bail!("Recovery snapshot chunk must contain 1–512 KiB");
                }
                let decoded_before = doc.decoded_bytes();
                let size = layer_tree_size_for_set(doc, *set_id)?;
                if !(64..=MAX_SIZE).contains(&size) {
                    bail!("Invalid recovery Texture Set resolution");
                }
                let area = u64::from(size)
                    .checked_mul(u64::from(size))
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot image size overflow"))?;
                let target_bytes = area
                    .checked_mul(if matches!(target, LayerImageTarget::Mask) {
                        1
                    } else {
                        4
                    })
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot target size overflow"))?;
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot chunk offset overflow"))?;
                if end > target_bytes {
                    bail!("Recovery snapshot chunk is outside its target buffer");
                }
                let start = usize::try_from(*offset)
                    .map_err(|_| anyhow::anyhow!("Recovery snapshot offset is not addressable"))?;
                let finish = start
                    .checked_add(bytes.len())
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot chunk range overflow"))?;
                let layer = root_paint_layer_mut(layer_tree_for_set_mut(doc, *set_id)?, *layer_id)?;
                let raw: &mut [u8] = match target {
                    LayerImageTarget::Mask => {
                        if layer.mask.dimensions() != (size, size)
                            || layer.mask.as_raw().len() as u64 != area
                        {
                            bail!("Recovery snapshot target mask dimensions are invalid");
                        }
                        layer.mask.as_mut()
                    }
                    LayerImageTarget::Channel(channel) => {
                        let total = area.checked_mul(4).ok_or_else(|| {
                            anyhow::anyhow!("Recovery snapshot channel size overflow")
                        })?;
                        if !layer.pixels.contains_key(channel) {
                            if !channel_allocation_fits(decoded_before, total) {
                                bail!("Recovery snapshot channel exceeds the 512-MiB decoded texture budget");
                            }
                            let len = usize::try_from(total).map_err(|_| {
                                anyhow::anyhow!("Recovery snapshot channel size is not addressable")
                            })?;
                            let mut pixels = Vec::new();
                            pixels.try_reserve_exact(len)?;
                            pixels.resize(len, 0);
                            let image =
                                RgbaImage::from_raw(size, size, pixels).ok_or_else(|| {
                                    anyhow::anyhow!("Could not allocate recovery snapshot channel")
                                })?;
                            layer.pixels.insert(*channel, image);
                        }
                        let image = layer
                            .pixels
                            .get_mut(channel)
                            .expect("inserted snapshot channel");
                        if image.dimensions() != (size, size)
                            || image.as_raw().len() as u64 != total
                        {
                            bail!("Recovery snapshot target channel dimensions are invalid");
                        }
                        image.as_mut()
                    }
                };
                raw.get_mut(start..finish)
                    .ok_or_else(|| anyhow::anyhow!("Recovery snapshot chunk range is invalid"))?
                    .copy_from_slice(bytes);
                return Ok(());
            }
            Self::LayerSnapshotCommit {
                set_id,
                layer_id,
                selected_after_id,
            } => {
                root_paint_index(layer_tree_for_set(doc, *set_id)?.0, *layer_id)?;
                layer_tree_for_set_mut(doc, *set_id)?.select(*selected_after_id)?;
                return Ok(());
            }
            Self::LayerTreeNodePropertiesSet {
                set_id,
                node_id,
                name,
                visible,
                opacity,
                blend,
                disabled_channels,
                procedural_graph,
            } => {
                if *set_id == 0 || *node_id == 0 {
                    bail!("Recovery tree property IDs must be nonzero");
                }
                if name.trim().is_empty() || name.len() > MAX_LAYER_NAME_BYTES {
                    bail!("Recovery node name must contain 1–4096 bytes");
                }
                if !opacity.is_finite() || !(0.0..=1.0).contains(opacity) {
                    bail!("Recovery node opacity must be finite and within 0..=1");
                }
                if disabled_channels
                    .iter()
                    .any(|channel| !Channel::ALL.contains(channel))
                {
                    bail!("Recovery node disabled channels are invalid");
                }
                if let Some(graph) = procedural_graph {
                    graph.validate()?;
                }
                let next_name = fallible_string(name)?;
                let mut next_disabled = HashSet::new();
                next_disabled.try_reserve(disabled_channels.len())?;
                next_disabled.extend(disabled_channels.iter().copied());
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                let node = tree
                    .find_node_mut(*node_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown recovery node {node_id}"))?;
                match node {
                    LayerNode::Paint(layer) => {
                        layer.name = next_name;
                        layer.visible = *visible;
                        layer.opacity = *opacity;
                        layer.blend = *blend;
                        layer.disabled_channels = next_disabled;
                        layer.procedural_graph = procedural_graph.clone();
                    }
                    LayerNode::Fill(fill) => {
                        if procedural_graph.is_some() {
                            bail!("Fill nodes cannot have procedural graphs");
                        }
                        fill.properties.name = next_name;
                        fill.properties.visible = *visible;
                        fill.properties.opacity = *opacity;
                        fill.properties.blend = *blend;
                        fill.properties.disabled_channels = next_disabled;
                    }
                    LayerNode::Group(group) => {
                        if procedural_graph.is_some() {
                            bail!("Group nodes cannot have procedural graphs");
                        }
                        group.properties.name = next_name;
                        group.properties.visible = *visible;
                        group.properties.opacity = *opacity;
                        group.properties.blend = *blend;
                        group.properties.disabled_channels = next_disabled;
                    }
                }
                return Ok(());
            }
            Self::LayerTreeFillValueSet {
                set_id,
                node_id,
                channel,
                value,
            } => {
                if *set_id == 0 || *node_id == 0 {
                    bail!("Recovery fill IDs must be nonzero");
                }
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                let node = tree
                    .find_node_mut(*node_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown recovery node {node_id}"))?;
                let LayerNode::Fill(fill) = node else {
                    bail!("Recovery fill value target is not a Fill node");
                };
                match value {
                    Some(value) => {
                        if !fill.values.contains_key(channel) {
                            fill.values.try_reserve(1)?;
                        }
                        fill.values.insert(*channel, *value);
                    }
                    None => {
                        if fill.values.len() <= 1 && fill.values.contains_key(channel) {
                            bail!("Recovery cannot remove the final Fill value");
                        }
                        if fill.values.remove(channel).is_none() {
                            bail!("Recovery Fill value to remove does not exist");
                        }
                    }
                }
                return Ok(());
            }
            Self::LayerTreeNodeMove {
                set_id,
                node_id,
                from_parent_id,
                from_index,
                to_parent_id,
                to_index,
            } => {
                if *set_id == 0 || *node_id == 0 {
                    bail!("Recovery tree move IDs must be nonzero");
                }
                let tree = layer_tree_for_set(doc, *set_id)?.0;
                if tree.parent_id(*node_id) != Some(*from_parent_id) {
                    bail!("Recovery tree move source parent precondition failed");
                }
                let siblings = tree.siblings(*from_parent_id)?;
                let from = usize::try_from(*from_index)?;
                if siblings.get(from).is_none_or(|node| node.id() != *node_id) {
                    bail!("Recovery tree move source index precondition failed");
                }
                let tree = layer_tree_for_set_mut(doc, *set_id)?;
                tree.move_node(*node_id, *to_parent_id, usize::try_from(*to_index)?)?;
                return Ok(());
            }
            Self::LayerTreeNodeRemove {
                set_id,
                node_id,
                expected_parent_id,
                expected_index,
                selected_after_node_id,
            } => {
                if *set_id == 0 || *node_id == 0 || *selected_after_node_id == 0 {
                    bail!("Recovery tree remove IDs must be nonzero");
                }
                let tree = layer_tree_for_set(doc, *set_id)?.0;
                if tree.parent_id(*node_id) != Some(*expected_parent_id) {
                    bail!("Recovery tree remove parent precondition failed");
                }
                let siblings = tree.siblings(*expected_parent_id)?;
                let index = usize::try_from(*expected_index)?;
                if siblings.get(index).is_none_or(|node| node.id() != *node_id) {
                    bail!("Recovery tree remove index precondition failed");
                }
                if tree.find_node(*selected_after_node_id).is_none() {
                    bail!("Recovery tree remove selection target does not exist");
                }
                layer_tree_for_set_mut(doc, *set_id)?.remove(*node_id, *selected_after_node_id)?;
                return Ok(());
            }
            Self::LayerTreeSubtreeBegin { .. }
            | Self::LayerTreeSubtreeChunk { .. }
            | Self::LayerTreeSubtreeCommit { .. } => {
                bail!("Layer tree subtree events must be applied through MeshRecoveryReplayer")
            }
            Self::TextureImportBegin { .. }
            | Self::TextureImportCommit { .. }
            | Self::LayerChannelBegin { .. }
            | Self::LayerChannelCommit { .. } => return Ok(()),
            Self::MeshReplaceBegin { .. }
            | Self::MeshReplaceChunk { .. }
            | Self::MeshReplaceCommit { .. } => {
                bail!("Mesh replacement events must be applied through MeshRecoveryReplayer")
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
        let size = layer_tree_size_for_set(doc, *set_id)?;
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
        let tree = layer_tree_for_set_mut(doc, *set_id)?;
        let node = tree
            .find_node_mut(*layer_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown recovery node {layer_id}"))?;
        match target {
            LayerImageTarget::Mask => {
                let mask = node_mask_mut(node);
                if mask.dimensions() != (size, size)
                    || mask.as_raw().len() != (size as usize).saturating_mul(size as usize)
                {
                    bail!("Recovery target mask dimensions do not match its Texture Set");
                }
                let target = mask.as_mut();
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
                let LayerNode::Paint(layer) = node else {
                    bail!("Recovery channel patch target is not a Paint node");
                };
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

struct MeshReplaceStaging {
    transaction_id: Uuid,
    encoded_len: usize,
    sha256: [u8; 32],
    material_bindings: Vec<Option<u32>>,
    bytes: Vec<u8>,
}

struct LayerTreeSubtreeStaging {
    transaction_id: Uuid,
    set_id: u32,
    root_node_id: u32,
    parent_id: Option<u32>,
    index: u32,
    selected_node_id: u32,
    node_count: usize,
    encoded_len: usize,
    sha256: [u8; 32],
    bytes: Vec<u8>,
}

/// Replays bounded mesh replacement transactions without exposing partial
/// mesh or material-binding state to the live document.
#[derive(Default)]
pub struct MeshRecoveryReplayer {
    staging: Option<MeshReplaceStaging>,
    tree_staging: Option<LayerTreeSubtreeStaging>,
}

impl MeshRecoveryReplayer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_event(&mut self, doc: &mut Document, event: DocumentEvent) -> Result<()> {
        if self.tree_staging.is_some() {
            return self.apply_tree_subtree_event(doc, event);
        }
        match event {
            DocumentEvent::MeshReplaceBegin {
                transaction_id,
                encoded_len,
                sha256,
                material_bindings,
            } => {
                if self.staging.is_some() {
                    bail!("Mesh replacement transactions cannot be nested");
                }
                if transaction_id.is_nil() {
                    bail!("Mesh replacement transaction ID cannot be nil");
                }
                if encoded_len == 0 || encoded_len > MAX_MESH_REPLACE_BYTES {
                    bail!("Mesh replacement payload must contain 1–32 MiB");
                }
                if material_bindings.is_empty() || material_bindings.len() > 256 {
                    bail!("Mesh replacement material binding count must be 1–256");
                }
                validate_mesh_bindings(doc, &material_bindings)?;
                let capacity = usize::try_from(encoded_len)
                    .map_err(|_| anyhow::anyhow!("Mesh replacement payload is not addressable"))?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(capacity)?;
                self.staging = Some(MeshReplaceStaging {
                    transaction_id,
                    encoded_len: capacity,
                    sha256,
                    material_bindings,
                    bytes,
                });
                Ok(())
            }
            DocumentEvent::MeshReplaceChunk {
                transaction_id,
                offset,
                bytes,
            } => {
                let open = self.staging.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("Mesh replacement chunk is outside a transaction")
                })?;
                if transaction_id != open.transaction_id {
                    bail!("Mesh replacement chunk transaction ID does not match its begin marker");
                }
                if bytes.is_empty() || bytes.len() > MAX_MESH_REPLACE_CHUNK_BYTES {
                    bail!("Mesh replacement chunk must contain 1–4 MiB");
                }
                if offset != open.bytes.len() as u64 {
                    bail!("Mesh replacement chunks must be contiguous from offset zero");
                }
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Mesh replacement chunk range overflow"))?;
                if end > open.encoded_len as u64 {
                    bail!("Mesh replacement chunk exceeds its declared payload length");
                }
                open.bytes.try_reserve_exact(bytes.len())?;
                open.bytes.extend_from_slice(&bytes);
                Ok(())
            }
            DocumentEvent::MeshReplaceCommit { transaction_id } => {
                let open = self.staging.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("Mesh replacement commit has no begin marker")
                })?;
                if transaction_id != open.transaction_id {
                    bail!("Mesh replacement commit transaction ID does not match its begin marker");
                }
                if open.bytes.len() != open.encoded_len {
                    bail!("Mesh replacement payload is incomplete");
                }
                let digest = Sha256::digest(&open.bytes);
                if digest.as_slice() != open.sha256 {
                    bail!("Mesh replacement SHA-256 does not match its begin marker");
                }
                let mesh: crate::mesh::Mesh = serde_json::from_slice(&open.bytes)
                    .map_err(|error| anyhow::anyhow!("Invalid mesh replacement JSON: {error}"))?;
                mesh.validate()?;
                if open.material_bindings.len() != mesh.material_count() {
                    bail!("Mesh replacement binding count does not match material slots");
                }
                validate_mesh_bindings(doc, &open.material_bindings)?;
                let bindings = std::mem::take(&mut self.staging).unwrap().material_bindings;
                doc.mesh = mesh;
                doc.material_bindings = bindings;
                Ok(())
            }
            DocumentEvent::LayerTreeSubtreeBegin {
                transaction_id,
                set_id,
                root_node_id,
                parent_id,
                index,
                selected_node_id,
                node_count,
                encoded_len,
                sha256,
            } => {
                if self.staging.is_some() {
                    bail!("Layer-tree subtree transactions cannot overlap mesh replacement");
                }
                validate_tree_subtree_begin(
                    doc,
                    transaction_id,
                    set_id,
                    root_node_id,
                    parent_id,
                    index,
                    selected_node_id,
                    node_count,
                    encoded_len,
                )?;
                let capacity = usize::try_from(encoded_len)
                    .map_err(|_| anyhow::anyhow!("Recovery subtree length is not addressable"))?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(capacity)?;
                self.tree_staging = Some(LayerTreeSubtreeStaging {
                    transaction_id,
                    set_id,
                    root_node_id,
                    parent_id,
                    index,
                    selected_node_id,
                    node_count: node_count as usize,
                    encoded_len: capacity,
                    sha256,
                    bytes,
                });
                Ok(())
            }
            DocumentEvent::LayerTreeSubtreeChunk { .. }
            | DocumentEvent::LayerTreeSubtreeCommit { .. } => {
                bail!("Layer-tree subtree chunk/commit is outside a transaction")
            }
            ordinary => {
                if self.staging.is_some() {
                    bail!("Recovery transaction contains an unrelated event");
                }
                ordinary.apply(doc)
            }
        }
    }

    fn apply_tree_subtree_event(&mut self, doc: &mut Document, event: DocumentEvent) -> Result<()> {
        match event {
            DocumentEvent::LayerTreeSubtreeChunk {
                transaction_id,
                offset,
                data,
            } => {
                let open = self
                    .tree_staging
                    .as_mut()
                    .expect("tree transaction is open");
                if transaction_id != open.transaction_id {
                    bail!("Recovery subtree chunk transaction ID does not match its begin marker");
                }
                let bytes = decode_tree_subtree_chunk(&data)?;
                if offset != open.bytes.len() as u64 {
                    bail!("Recovery subtree chunks must be contiguous from offset zero");
                }
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("Recovery subtree chunk range overflow"))?;
                if end > open.encoded_len as u64 {
                    bail!("Recovery subtree chunk exceeds its declared payload length");
                }
                open.bytes.try_reserve_exact(bytes.len())?;
                open.bytes.extend_from_slice(&bytes);
                Ok(())
            }
            DocumentEvent::LayerTreeSubtreeCommit { transaction_id } => {
                let open = self
                    .tree_staging
                    .as_ref()
                    .expect("tree transaction is open");
                if transaction_id != open.transaction_id {
                    bail!("Recovery subtree commit ID does not match its begin marker");
                }
                if open.bytes.len() != open.encoded_len {
                    bail!("Recovery subtree payload is incomplete");
                }
                if Sha256::digest(&open.bytes).as_slice() != open.sha256 {
                    bail!("Recovery subtree SHA-256 does not match its begin marker");
                }
                let staged = self.tree_staging.take().expect("tree transaction is open");
                attach_staged_subtree(doc, staged)
            }
            _ => bail!("Recovery subtree transaction contains an unrelated event"),
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.staging.is_some() {
            bail!("Recovery ended during a mesh replacement transaction");
        }
        if self.tree_staging.is_some() {
            bail!("Recovery ended during a layer-tree subtree transaction");
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_tree_subtree_begin(
    doc: &Document,
    transaction_id: Uuid,
    set_id: u32,
    root_node_id: u32,
    parent_id: Option<u32>,
    index: u32,
    selected_node_id: u32,
    node_count: u32,
    encoded_len: u64,
) -> Result<()> {
    if transaction_id.is_nil()
        || set_id == 0
        || root_node_id == 0
        || selected_node_id == 0
        || node_count == 0
        || node_count as usize > MAX_LAYER_TREE_NODES
        || encoded_len == 0
        || encoded_len > MAX_LAYER_TREE_SUBTREE_BYTES as u64
    {
        bail!("Recovery subtree begin marker is out of bounds");
    }
    let (tree, size) = layer_tree_for_set(doc, set_id)?;
    tree.validate(size)?;
    if tree.find_node(root_node_id).is_some() {
        bail!("Recovery subtree root ID already exists");
    }
    if let Some(parent_id) = parent_id {
        if !matches!(tree.find_node(parent_id), Some(LayerNode::Group(_))) {
            bail!("Recovery subtree parent is missing or is not a Group");
        }
    }
    if index as usize > tree.siblings(parent_id)?.len() {
        bail!("Recovery subtree insertion index is out of range");
    }
    Ok(())
}

fn attach_staged_subtree(doc: &mut Document, staged: LayerTreeSubtreeStaging) -> Result<()> {
    let snapshot: TreeSubtreeSnapshot = serde_json::from_slice(&staged.bytes)
        .map_err(|error| anyhow::anyhow!("Invalid recovery subtree JSON: {error}"))?;
    let (_, size) = layer_tree_for_set(doc, staged.set_id)?;
    let (subtree_bytes, subtree_extensions) =
        validate_tree_snapshot(&snapshot, staged.root_node_id, staged.node_count, size)?;
    let global_bytes = doc.checked_decoded_bytes()?;
    if global_bytes
        .checked_add(subtree_bytes)
        .is_none_or(|total| total > MAX_DECODED_BYTES)
    {
        bail!("Recovery subtree exceeds the 512-MiB decoded texture budget");
    }
    let existing_extensions = layer_extension_bytes(doc)?;
    if existing_extensions
        .checked_add(subtree_extensions)
        .is_none_or(|total| total > MAX_LAYER_EXTENSION_BYTES)
    {
        bail!("Recovery subtree exceeds the 48-MiB aggregate layer extension limit");
    }

    let resulting_selection_in_subtree = snapshot
        .nodes
        .iter()
        .any(|record| snapshot_node_id(&record.kind) == staged.selected_node_id);
    let size = {
        let (_, size) = layer_tree_for_set(doc, staged.set_id)?;
        size
    };
    let node = build_subtree(snapshot.nodes, size)?;
    let tree = layer_tree_for_set_mut(doc, staged.set_id)?;
    if !resulting_selection_in_subtree && tree.find_node(staged.selected_node_id).is_none() {
        bail!("Recovery subtree selected node ID is unknown");
    }
    tree.insert(staged.parent_id, staged.index as usize, node)?;
    tree.select(staged.selected_node_id)?;
    tree.validate(size)?;
    Ok(())
}

fn validate_mesh_bindings(doc: &Document, bindings: &[Option<u32>]) -> Result<()> {
    if bindings.is_empty() || bindings.len() > 256 {
        bail!("Mesh replacement material binding count must be 1–256");
    }
    for id in bindings.iter().flatten() {
        if *id == 0
            || (*id != doc.active_set_id && !doc.texture_sets.iter().any(|set| set.id == *id))
        {
            bail!("Mesh replacement binding references unknown Texture Set {id}");
        }
    }
    Ok(())
}

const MAX_LAYER_NAME_BYTES: usize = 1024;

fn layer_tree_for_set(doc: &Document, set_id: u32) -> Result<(&LayerTree, u32)> {
    if set_id == 0 {
        bail!("Recovery Texture Set ID must be nonzero");
    }
    if set_id == doc.active_set_id {
        return Ok((&doc.layer_tree, doc.size));
    }
    let set = doc
        .texture_sets
        .iter()
        .find(|set| set.id == set_id)
        .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?;
    Ok((&set.layer_tree, set.size))
}

fn layer_tree_for_set_mut(doc: &mut Document, set_id: u32) -> Result<&mut LayerTree> {
    if set_id == 0 {
        bail!("Recovery Texture Set ID must be nonzero");
    }
    if set_id == doc.active_set_id {
        return Ok(&mut doc.layer_tree);
    }
    let set = doc
        .texture_sets
        .iter_mut()
        .find(|set| set.id == set_id)
        .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?;
    Ok(&mut set.layer_tree)
}

fn layer_tree_size_for_set(doc: &Document, set_id: u32) -> Result<u32> {
    layer_tree_for_set(doc, set_id).map(|(_, size)| size)
}

fn root_paint_index(tree: &LayerTree, layer_id: u32) -> Result<usize> {
    if layer_id == 0 {
        bail!("Recovery layer ID must be nonzero");
    }
    let mut found = None;
    for (index, node) in tree.roots.iter().enumerate() {
        if node.id() == layer_id {
            if found.is_some() || !matches!(node, LayerNode::Paint(_)) {
                bail!("Recovery legacy layer ID is ambiguous or not a root Paint node");
            }
            found = Some(index);
        }
    }
    found.ok_or_else(|| anyhow::anyhow!("Unknown legacy recovery layer {layer_id}"))
}

fn root_paint_index_mut(tree: &LayerTree, layer_id: u32) -> Result<usize> {
    root_paint_index(tree, layer_id)
}

fn root_paint_layer(tree: &LayerTree, layer_id: u32) -> Result<&Layer> {
    let index = root_paint_index(tree, layer_id)?;
    match &tree.roots[index] {
        LayerNode::Paint(layer) => Ok(layer),
        _ => unreachable!("root_paint_index guarantees a Paint node"),
    }
}

fn root_paint_layer_mut(tree: &mut LayerTree, layer_id: u32) -> Result<&mut Layer> {
    let index = root_paint_index(tree, layer_id)?;
    match &mut tree.roots[index] {
        LayerNode::Paint(layer) => Ok(layer),
        _ => unreachable!("root_paint_index guarantees a Paint node"),
    }
}

fn node_properties_mut(node: &mut LayerNode) -> Option<&mut LayerNodeProperties> {
    match node {
        LayerNode::Paint(_) => None,
        LayerNode::Fill(fill) => Some(&mut fill.properties),
        LayerNode::Group(group) => Some(&mut group.properties),
    }
}

fn node_mask_mut(node: &mut LayerNode) -> &mut GrayImage {
    match node {
        LayerNode::Paint(layer) => &mut layer.mask,
        LayerNode::Fill(fill) => &mut fill.properties.mask,
        LayerNode::Group(group) => &mut group.properties.mask,
    }
}

fn validate_snapshot_channels(channels: &[Channel]) -> Result<()> {
    let mut prior = None;
    for channel in channels {
        let rank = Channel::ALL
            .iter()
            .position(|candidate| candidate == channel)
            .ok_or_else(|| anyhow::anyhow!("Unknown recovery snapshot channel"))?;
        if prior.is_some_and(|value| rank <= value) {
            bail!("Recovery snapshot channels must be unique and in Channel::ALL order");
        }
        prior = Some(rank);
    }
    Ok(())
}

fn validate_snapshot_disabled_channels(channels: &[Channel]) -> Result<()> {
    let mut seen = HashSet::new();
    seen.try_reserve(channels.len())?;
    for channel in channels {
        if !Channel::ALL.contains(channel) || !seen.insert(*channel) {
            bail!("Recovery snapshot disabled channels are invalid or duplicated");
        }
    }
    Ok(())
}

fn exact_texture_size(area: u64) -> Option<u32> {
    let root = (area as f64).sqrt() as u64;
    (root.checked_mul(root) == Some(area) && (64..=u64::from(MAX_SIZE)).contains(&root))
        .then_some(root as u32)
}

fn texture_set_name_mut(doc: &mut Document, set_id: u32) -> Result<&mut String> {
    if set_id == 0 {
        bail!("Recovery Texture Set ID must be nonzero");
    }
    if set_id == doc.active_set_id {
        return Ok(&mut doc.active_set_name);
    }
    Ok(&mut doc
        .texture_sets
        .iter_mut()
        .find(|set| set.id == set_id)
        .ok_or_else(|| anyhow::anyhow!("Unknown recovery Texture Set {set_id}"))?
        .name)
}

fn layer_allocation_fits(decoded_bytes_before: u64, allocation_bytes: u64) -> bool {
    decoded_bytes_before
        .checked_add(allocation_bytes)
        .is_some_and(|used| used <= MAX_DECODED_BYTES)
}

fn texture_set_allocation_fits(decoded_bytes_before: u64, size: u32) -> bool {
    u64::from(size)
        .checked_mul(u64::from(size))
        .is_some_and(|mask_bytes| layer_allocation_fits(decoded_bytes_before, mask_bytes))
}

fn fallible_string(value: &str) -> Result<String> {
    let mut result = String::new();
    result.try_reserve_exact(value.len())?;
    result.push_str(value);
    Ok(result)
}

fn blank_layer_fallible(id: u32, name: &str, size: u32) -> Result<Layer> {
    let mask_bytes = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Recovery layer mask size overflow"))?;
    let mask_len = usize::try_from(mask_bytes)
        .map_err(|_| anyhow::anyhow!("Recovery layer mask size is not addressable"))?;
    let mut mask_pixels = Vec::new();
    mask_pixels.try_reserve_exact(mask_len)?;
    mask_pixels.resize(mask_len, 255);
    let mask = GrayImage::from_raw(size, size, mask_pixels)
        .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery layer mask"))?;
    Ok(Layer {
        id,
        name: fallible_string(name)?,
        visible: true,
        opacity: 1.0,
        blend: BlendMode::Normal,
        pixels: Default::default(),
        disabled_channels: Default::default(),
        mask,
        procedural_graph: None,
        extensions: Default::default(),
    })
}

fn layer_storage_bytes_checked(layer: &Layer, size: u32) -> Result<u64> {
    let expected_mask_bytes = u64::from(size)
        .checked_mul(u64::from(size))
        .ok_or_else(|| anyhow::anyhow!("Recovery layer mask size overflow"))?;
    if layer.mask.dimensions() != (size, size)
        || layer.mask.as_raw().len() as u64 != expected_mask_bytes
    {
        bail!("Recovery source layer mask dimensions are invalid");
    }

    let expected_channel_bytes = expected_mask_bytes
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("Recovery layer channel size overflow"))?;
    let mut total = expected_mask_bytes;
    for image in layer.pixels.values() {
        if image.dimensions() != (size, size)
            || image.as_raw().len() as u64 != expected_channel_bytes
        {
            bail!("Recovery source layer channel dimensions are invalid");
        }
        total = total
            .checked_add(expected_channel_bytes)
            .ok_or_else(|| anyhow::anyhow!("Recovery layer storage size overflow"))?;
    }
    Ok(total)
}

fn duplicate_layer_fallible(source: &Layer, id: u32, size: u32) -> Result<Layer> {
    if source.name.is_empty() || source.name.len() > 1024 {
        bail!("Recovery source layer name is invalid");
    }
    let name_len = source
        .name
        .len()
        .checked_add(" copy".len())
        .ok_or_else(|| anyhow::anyhow!("Recovery duplicate layer name size overflow"))?;
    if name_len > 1024 {
        bail!("Recovery duplicate layer name exceeds 1024 bytes");
    }
    let mut name = String::new();
    name.try_reserve_exact(name_len)?;
    name.push_str(&source.name);
    name.push_str(" copy");

    let mask_len = usize::try_from(
        u64::from(size)
            .checked_mul(u64::from(size))
            .ok_or_else(|| anyhow::anyhow!("Recovery layer mask size overflow"))?,
    )
    .map_err(|_| anyhow::anyhow!("Recovery layer mask size is not addressable"))?;
    let mut mask_bytes = Vec::new();
    mask_bytes.try_reserve_exact(mask_len)?;
    mask_bytes.extend_from_slice(source.mask.as_raw());
    let mask = GrayImage::from_raw(size, size, mask_bytes)
        .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery duplicate mask"))?;

    let mut pixels = HashMap::new();
    pixels.try_reserve(source.pixels.len())?;
    for (channel, image) in &source.pixels {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(image.as_raw().len())?;
        bytes.extend_from_slice(image.as_raw());
        let copy = RgbaImage::from_raw(size, size, bytes)
            .ok_or_else(|| anyhow::anyhow!("Could not allocate recovery duplicate channel"))?;
        pixels.insert(*channel, copy);
    }

    let mut disabled_channels = HashSet::new();
    disabled_channels.try_reserve(source.disabled_channels.len())?;
    disabled_channels.extend(source.disabled_channels.iter().copied());

    Ok(Layer {
        id,
        name,
        visible: source.visible,
        opacity: source.opacity,
        blend: source.blend,
        pixels,
        disabled_channels,
        mask,
        procedural_graph: source.procedural_graph.clone(),
        // serde_json::Map is a BTreeMap in this build and has no fallible reserve API.
        // The serialized source is bounded before this pre-mutation clone.
        extensions: source.extensions.clone(),
    })
}

fn extension_serialized_size(
    extensions: &serde_json::Map<String, serde_json::Value>,
    max_bytes: usize,
) -> Result<usize> {
    let mut writer = CountingWriter {
        bytes_written: 0,
        max_bytes,
    };
    serde_json::to_writer(&mut writer, extensions)?;
    Ok(writer.bytes_written)
}

fn layer_extension_bytes(doc: &Document) -> Result<usize> {
    let mut total = 0usize;
    let mut trees = Vec::new();
    trees.try_reserve_exact(doc.texture_sets.len().saturating_add(1))?;
    trees.push(&doc.layer_tree);
    trees.extend(doc.texture_sets.iter().map(|set| &set.layer_tree));
    for tree in trees {
        for id in tree.all_node_ids() {
            let node = tree
                .find_node(id)
                .ok_or_else(|| anyhow::anyhow!("Layer tree traversal returned a missing node"))?;
            let extensions = match node {
                LayerNode::Paint(layer) => &layer.extensions,
                LayerNode::Fill(fill) => &fill.properties.extensions,
                LayerNode::Group(group) => &group.properties.extensions,
            };
            let remaining = MAX_LAYER_EXTENSION_BYTES.saturating_sub(total);
            total = total
                .checked_add(extension_serialized_size(extensions, remaining)?)
                .ok_or_else(|| anyhow::anyhow!("Recovery extension byte count overflow"))?;
            if total > MAX_LAYER_EXTENSION_BYTES {
                bail!("Recovery layer extensions exceed the 48-MiB aggregate limit");
            }
        }
    }
    Ok(total)
}

struct CountingWriter {
    bytes_written: usize,
    max_bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes_written
            .checked_add(bytes.len())
            .filter(|&length| length <= self.max_bytes)
            .ok_or_else(|| io::Error::other("layer extensions exceed the clone limit"))?;
        self.bytes_written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode an ordered batch as one JSON array, keeping the entire atomic record
/// within the journal payload limit.
pub fn encode_event_batch_bounded(events: &[DocumentEvent]) -> io::Result<Vec<u8>> {
    if events.iter().any(|event| {
        matches!(event, DocumentEvent::LayerSnapshotChunk { bytes, .. }
            if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_CHUNK_BYTES)
    }) {
        return Err(invalid_input("snapshot chunk must contain 1–512 KiB"));
    }
    if events.iter().any(|event| {
        matches!(event, DocumentEvent::MeshReplaceChunk { bytes, .. }
            if bytes.is_empty() || bytes.len() > MAX_MESH_REPLACE_CHUNK_BYTES)
    }) {
        return Err(invalid_input("mesh recovery chunk must contain 1–4 MiB"));
    }
    for event in events {
        validate_tree_subtree_event_bounds(event)
            .map_err(|error| invalid_data_owned(error.to_string()))?;
    }
    let mut writer = BoundedWriter::new(MAX_PAYLOAD_BYTES);
    serde_json::to_writer(&mut writer, events)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(writer.bytes)
}

fn decode_tree_subtree_chunk(data: &str) -> Result<Vec<u8>> {
    let max_encoded = MAX_LAYER_TREE_SUBTREE_CHUNK_BYTES.div_ceil(3) * 4;
    if data.is_empty() || data.len() > max_encoded {
        bail!("Layer-tree recovery chunk must contain 1–256 KiB raw");
    }
    let bytes = base64::decode(data)
        .map_err(|error| anyhow::anyhow!("Invalid layer-tree recovery chunk: {error}"))?;
    if bytes.is_empty() || bytes.len() > MAX_LAYER_TREE_SUBTREE_CHUNK_BYTES {
        bail!("Layer-tree recovery chunk must contain 1–256 KiB raw");
    }
    Ok(bytes)
}

fn validate_tree_subtree_event_bounds(event: &DocumentEvent) -> Result<()> {
    match event {
        DocumentEvent::LayerTreeSubtreeBegin {
            transaction_id,
            set_id,
            root_node_id,
            parent_id,
            index,
            selected_node_id,
            node_count,
            encoded_len,
            ..
        } => {
            if transaction_id.is_nil()
                || *set_id == 0
                || *root_node_id == 0
                || parent_id.is_some_and(|id| id == 0)
                || *index as usize > MAX_LAYER_TREE_NODES
                || *selected_node_id == 0
                || *node_count == 0
                || *node_count as usize > MAX_LAYER_TREE_NODES
                || *encoded_len == 0
                || *encoded_len > MAX_LAYER_TREE_SUBTREE_BYTES as u64
            {
                bail!("Layer-tree subtree begin marker is out of bounds");
            }
        }
        DocumentEvent::LayerTreeSubtreeChunk {
            transaction_id,
            offset,
            data,
        } => {
            if transaction_id.is_nil()
                || *offset > MAX_LAYER_TREE_SUBTREE_BYTES as u64
                || decode_tree_subtree_chunk(data)?.is_empty()
            {
                bail!("Layer-tree subtree chunk marker is invalid");
            }
        }
        DocumentEvent::LayerTreeSubtreeCommit { transaction_id } if transaction_id.is_nil() => {
            bail!("Layer-tree subtree commit ID must be nonzero");
        }
        _ => {}
    }
    Ok(())
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

/// Return the number of records that end on a complete channel-layer
/// transaction boundary. Incomplete final imports/bakes are excluded while
/// every earlier complete record is retained. Malformed marker sequences and
/// transaction bodies fail closed.
pub fn committed_record_count(records: &[Record]) -> Result<usize> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TransactionKind {
        Imported,
        Generated,
    }

    #[derive(Clone, Copy)]
    struct ImportTransaction {
        start_record: usize,
        kind: TransactionKind,
        set_id: u32,
        layer_id: u32,
        channel: Channel,
        created_layer: bool,
        patched_channel: bool,
        selected_layer: bool,
    }

    struct SnapshotTransaction {
        start_record: usize,
        set_id: u32,
        layer_id: u32,
        selected_after_id: u32,
        channels: Vec<Channel>,
        target_index: usize,
        offset: u64,
        size: Option<u32>,
    }

    struct MeshTransaction {
        start_record: usize,
        transaction_id: Uuid,
        encoded_len: u64,
        sha256: [u8; 32],
        offset: u64,
        hasher: Sha256,
    }

    struct TreeSubtreeTransaction {
        start_record: usize,
        transaction_id: Uuid,
        node_count: u32,
        encoded_len: u64,
        sha256: [u8; 32],
        offset: u64,
        hasher: Sha256,
    }

    let mut transaction: Option<ImportTransaction> = None;
    let mut snapshot: Option<SnapshotTransaction> = None;
    let mut mesh: Option<MeshTransaction> = None;
    let mut tree_subtree: Option<TreeSubtreeTransaction> = None;
    for (record_index, record) in records.iter().enumerate() {
        let events = decode_event_batch_bounded(&record.payload)?;
        for event in events {
            match event {
                DocumentEvent::TextureImportBegin {
                    set_id,
                    layer_id,
                    channel,
                } => {
                    if transaction.is_some()
                        || snapshot.is_some()
                        || mesh.is_some()
                        || tree_subtree.is_some()
                    {
                        bail!("Recovery layer-channel transactions cannot be nested");
                    }
                    if set_id == 0 || layer_id == 0 {
                        bail!("Recovery layer-channel transaction IDs must be nonzero");
                    }
                    transaction = Some(ImportTransaction {
                        start_record: record_index,
                        kind: TransactionKind::Imported,
                        set_id,
                        layer_id,
                        channel,
                        created_layer: false,
                        patched_channel: false,
                        selected_layer: false,
                    });
                }
                DocumentEvent::LayerChannelBegin {
                    set_id,
                    layer_id,
                    channel,
                } => {
                    if transaction.is_some()
                        || snapshot.is_some()
                        || mesh.is_some()
                        || tree_subtree.is_some()
                    {
                        bail!("Recovery layer-channel transactions cannot be nested");
                    }
                    if set_id == 0 || layer_id == 0 {
                        bail!("Recovery layer-channel transaction IDs must be nonzero");
                    }
                    transaction = Some(ImportTransaction {
                        start_record: record_index,
                        kind: TransactionKind::Generated,
                        set_id,
                        layer_id,
                        channel,
                        created_layer: false,
                        patched_channel: false,
                        selected_layer: false,
                    });
                }
                DocumentEvent::MeshReplaceBegin {
                    transaction_id,
                    encoded_len,
                    sha256,
                    material_bindings,
                } => {
                    if transaction.is_some()
                        || snapshot.is_some()
                        || mesh.is_some()
                        || tree_subtree.is_some()
                    {
                        bail!("Recovery transactions cannot be nested");
                    }
                    if transaction_id.is_nil()
                        || encoded_len == 0
                        || encoded_len > MAX_MESH_REPLACE_BYTES
                    {
                        bail!("Mesh replacement begin marker has invalid ID or payload length");
                    }
                    if material_bindings.is_empty() || material_bindings.len() > 256 {
                        bail!("Mesh replacement binding count must be 1–256");
                    }
                    mesh = Some(MeshTransaction {
                        start_record: record_index,
                        transaction_id,
                        encoded_len,
                        sha256,
                        offset: 0,
                        hasher: Sha256::new(),
                    });
                }
                DocumentEvent::MeshReplaceChunk {
                    transaction_id,
                    offset,
                    bytes,
                } => {
                    let open = mesh.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("Mesh replacement chunk is outside a transaction")
                    })?;
                    if transaction_id != open.transaction_id {
                        bail!(
                            "Mesh replacement chunk transaction ID does not match its begin marker"
                        );
                    }
                    if bytes.is_empty() || bytes.len() > MAX_MESH_REPLACE_CHUNK_BYTES {
                        bail!("Mesh replacement chunk must contain 1–4 MiB");
                    }
                    if offset != open.offset {
                        bail!("Mesh replacement chunks must be contiguous from offset zero");
                    }
                    let end = offset
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Mesh replacement chunk range overflow"))?;
                    if end > open.encoded_len {
                        bail!("Mesh replacement chunk exceeds its declared payload length");
                    }
                    open.hasher.update(bytes);
                    open.offset = end;
                }
                DocumentEvent::MeshReplaceCommit { transaction_id } => {
                    let open = mesh.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Mesh replacement commit has no begin marker")
                    })?;
                    if transaction_id != open.transaction_id {
                        bail!("Mesh replacement commit ID does not match its begin marker");
                    }
                    if open.offset != open.encoded_len {
                        bail!("Mesh replacement payload is incomplete");
                    }
                    if open.hasher.clone().finalize().as_slice() != open.sha256 {
                        bail!("Mesh replacement SHA-256 does not match its begin marker");
                    }
                    mesh = None;
                }
                DocumentEvent::LayerTreeSubtreeBegin {
                    transaction_id,
                    set_id,
                    root_node_id,
                    parent_id,
                    index,
                    selected_node_id,
                    node_count,
                    encoded_len,
                    sha256,
                } => {
                    if transaction.is_some()
                        || snapshot.is_some()
                        || mesh.is_some()
                        || tree_subtree.is_some()
                    {
                        bail!("Recovery transactions cannot be nested");
                    }
                    let event = DocumentEvent::LayerTreeSubtreeBegin {
                        transaction_id,
                        set_id,
                        root_node_id,
                        parent_id,
                        index,
                        selected_node_id,
                        node_count,
                        encoded_len,
                        sha256,
                    };
                    validate_tree_subtree_event_bounds(&event)?;
                    tree_subtree = Some(TreeSubtreeTransaction {
                        start_record: record_index,
                        transaction_id,
                        node_count,
                        encoded_len,
                        sha256,
                        offset: 0,
                        hasher: Sha256::new(),
                    });
                }
                DocumentEvent::LayerTreeSubtreeChunk {
                    transaction_id,
                    offset,
                    data,
                } => {
                    let open = tree_subtree.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("Layer-tree subtree chunk is outside a transaction")
                    })?;
                    if transaction_id != open.transaction_id {
                        bail!("Layer-tree subtree chunk transaction ID does not match its begin marker");
                    }
                    let bytes = decode_tree_subtree_chunk(&data)?;
                    if offset != open.offset {
                        bail!("Layer-tree subtree chunks must be contiguous from offset zero");
                    }
                    let end = offset.checked_add(bytes.len() as u64).ok_or_else(|| {
                        anyhow::anyhow!("Layer-tree subtree chunk range overflow")
                    })?;
                    if end > open.encoded_len {
                        bail!("Layer-tree subtree chunk exceeds its declared payload length");
                    }
                    open.hasher.update(&bytes);
                    open.offset = end;
                }
                DocumentEvent::LayerTreeSubtreeCommit { transaction_id } => {
                    let open = tree_subtree.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Layer-tree subtree commit has no begin marker")
                    })?;
                    if transaction_id != open.transaction_id {
                        bail!("Layer-tree subtree commit ID does not match its begin marker");
                    }
                    if open.offset != open.encoded_len {
                        bail!("Layer-tree subtree payload is incomplete");
                    }
                    if open.hasher.clone().finalize().as_slice() != open.sha256 {
                        bail!("Layer-tree subtree SHA-256 does not match its begin marker");
                    }
                    if open.node_count == 0 || open.node_count as usize > MAX_LAYER_TREE_NODES {
                        bail!("Layer-tree subtree node count is invalid");
                    }
                    tree_subtree = None;
                }
                DocumentEvent::LayerSnapshotBegin {
                    set_id,
                    layer_id,
                    selected_after_id,
                    channels,
                    ..
                } => {
                    if transaction.is_some()
                        || snapshot.is_some()
                        || mesh.is_some()
                        || tree_subtree.is_some()
                    {
                        bail!("Recovery layer transactions cannot be nested");
                    }
                    if set_id == 0 || layer_id == 0 || selected_after_id == 0 {
                        bail!("Recovery snapshot IDs must be nonzero");
                    }
                    validate_snapshot_channels(&channels)?;
                    snapshot = Some(SnapshotTransaction {
                        start_record: record_index,
                        set_id,
                        layer_id,
                        selected_after_id,
                        channels,
                        target_index: 0,
                        offset: 0,
                        size: None,
                    });
                }
                DocumentEvent::LayerSnapshotChunk {
                    set_id,
                    layer_id,
                    target,
                    offset,
                    bytes,
                } => {
                    let open = snapshot.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("Snapshot chunk is outside a snapshot transaction")
                    })?;
                    if (set_id, layer_id) != (open.set_id, open.layer_id) {
                        bail!("Recovery snapshot chunk does not match its begin marker");
                    }
                    if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_CHUNK_BYTES {
                        bail!("Recovery snapshot chunk must contain 1–512 KiB");
                    }
                    let mut expected_target = if open.target_index == 0 {
                        LayerImageTarget::Mask
                    } else {
                        LayerImageTarget::Channel(open.channels[open.target_index - 1])
                    };
                    if target != expected_target {
                        let size = open
                            .size
                            .or_else(|| exact_texture_size(open.offset))
                            .ok_or_else(|| {
                                anyhow::anyhow!("Recovery snapshot target buffer is incomplete")
                            })?;
                        let expected =
                            u64::from(size).pow(2) * if open.target_index == 0 { 1 } else { 4 };
                        if open.offset != expected || open.target_index >= open.channels.len() {
                            bail!("Recovery snapshot chunks must be contiguous and target-ordered");
                        }
                        open.size = Some(size);
                        open.target_index += 1;
                        open.offset = 0;
                        expected_target =
                            LayerImageTarget::Channel(open.channels[open.target_index - 1]);
                    }
                    if target != expected_target || offset != open.offset {
                        bail!("Recovery snapshot chunks must be contiguous and target-ordered");
                    }
                    let end = offset
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("Recovery snapshot chunk range overflow"))?;
                    let max_target = if open.target_index == 0 {
                        u64::from(MAX_SIZE).pow(2)
                    } else {
                        u64::from(open.size.unwrap()).pow(2) * 4
                    };
                    if end > max_target {
                        bail!("Recovery snapshot chunk exceeds its target buffer");
                    }
                    open.offset = end;
                }
                DocumentEvent::LayerSnapshotCommit {
                    set_id,
                    layer_id,
                    selected_after_id,
                } => {
                    let open = snapshot.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Unmatched recovery snapshot commit marker")
                    })?;
                    if (set_id, layer_id, selected_after_id)
                        != (open.set_id, open.layer_id, open.selected_after_id)
                    {
                        bail!("Recovery snapshot commit marker does not match its begin marker");
                    }
                    let size = open
                        .size
                        .or_else(|| exact_texture_size(open.offset))
                        .ok_or_else(|| {
                            anyhow::anyhow!("Recovery snapshot mask is incomplete or invalid")
                        })?;
                    let expected =
                        u64::from(size).pow(2) * if open.target_index == 0 { 1 } else { 4 };
                    if open.offset != expected {
                        bail!("Recovery snapshot target buffer is incomplete");
                    }
                    if open.target_index != open.channels.len() {
                        bail!("Recovery snapshot is missing one or more channel buffers");
                    }
                    snapshot = None;
                }
                DocumentEvent::TextureImportCommit {
                    set_id,
                    layer_id,
                    channel,
                } => {
                    if snapshot.is_some() || mesh.is_some() || tree_subtree.is_some() {
                        bail!("Recovery snapshot transaction contains an unexpected event");
                    }
                    let open = transaction
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Unmatched layer-channel commit marker"))?;
                    if open.kind != TransactionKind::Imported
                        || (set_id, layer_id, channel) != (open.set_id, open.layer_id, open.channel)
                    {
                        bail!(
                            "Recovery layer-channel commit marker does not match its begin marker"
                        );
                    }
                    if !open.created_layer || !open.patched_channel {
                        bail!("Recovery layer-channel transaction is missing its layer or channel data");
                    }
                    transaction = None;
                }
                DocumentEvent::LayerChannelCommit {
                    set_id,
                    layer_id,
                    channel,
                } => {
                    if snapshot.is_some() || mesh.is_some() || tree_subtree.is_some() {
                        bail!("Recovery snapshot transaction contains an unexpected event");
                    }
                    let open = transaction
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Unmatched layer-channel commit marker"))?;
                    if open.kind != TransactionKind::Generated
                        || (set_id, layer_id, channel) != (open.set_id, open.layer_id, open.channel)
                    {
                        bail!(
                            "Recovery layer-channel commit marker does not match its begin marker"
                        );
                    }
                    if !open.created_layer || !open.patched_channel {
                        bail!("Recovery layer-channel transaction is missing its layer or channel data");
                    }
                    transaction = None;
                }
                other => {
                    if snapshot.is_some() || mesh.is_some() || tree_subtree.is_some() {
                        bail!("Recovery snapshot transaction contains an unexpected event");
                    }
                    let Some(open) = transaction.as_mut() else {
                        continue;
                    };
                    match other {
                        DocumentEvent::LayerCreate {
                            set_id, layer_id, ..
                        } if set_id == open.set_id
                            && layer_id == open.layer_id
                            && !open.created_layer
                            && !open.patched_channel
                            && !open.selected_layer =>
                        {
                            open.created_layer = true;
                        }
                        DocumentEvent::LayerImagePatch {
                            set_id,
                            layer_id,
                            target: LayerImageTarget::Channel(channel),
                            ..
                        } if set_id == open.set_id
                            && layer_id == open.layer_id
                            && channel == open.channel
                            && open.created_layer
                            && !open.selected_layer =>
                        {
                            open.patched_channel = true;
                        }
                        DocumentEvent::LayerSelection { set_id, layer_id }
                            if set_id == open.set_id
                                && layer_id == open.layer_id
                                && open.created_layer
                                && open.patched_channel
                                && !open.selected_layer =>
                        {
                            open.selected_layer = true;
                        }
                        _ => bail!("Recovery layer-channel transaction contains an unrelated or out-of-order event"),
                    }
                }
            }
        }
    }

    Ok(transaction.map_or_else(
        || {
            snapshot.map_or_else(
                || {
                    mesh.map_or_else(
                        || tree_subtree.map_or(records.len(), |open| open.start_record),
                        |open| open.start_record,
                    )
                },
                |open| open.start_record,
            )
        },
        |open| open.start_record,
    ))
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
        if self.next_sequence > MAX_RECORDS as u64 {
            return Err(invalid_input(
                "journal exceeds 1,000,000 record replay limit",
            ));
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
        let sequence_bytes = sequence.to_le_bytes();
        let payload_len_bytes = (payload.len() as u32).to_le_bytes();
        let checksum = crc32_parts(&[&sequence_bytes, &payload_len_bytes, payload]);

        let result = (|| {
            self.file.write_all(&(body_len as u32).to_le_bytes())?;
            self.file.write_all(&sequence_bytes)?;
            self.file.write_all(&payload_len_bytes)?;
            self.file.write_all(payload)?;
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

    /// Truncate the on-disk journal to a previously scanned record prefix and
    /// reset this handle so subsequent appends continue at the prefix's next
    /// sequence. `records` must be the validated scan result for this journal.
    pub fn truncate_records(&mut self, records: &[Record], keep_count: usize) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "journal handle is poisoned after a prior write failure",
            ));
        }
        if keep_count > records.len() {
            return Err(invalid_input(
                "journal prefix length exceeds the validated record count",
            ));
        }

        let mut prefix_len = HEADER_LEN as u64;
        for (index, record) in records[..keep_count].iter().enumerate() {
            let expected_sequence = (index as u64)
                .checked_add(1)
                .ok_or_else(|| invalid_input("journal sequence number overflow"))?;
            if record.sequence != expected_sequence {
                return Err(invalid_input(
                    "journal prefix records are not in sequence order",
                ));
            }
            if record.payload.len() > MAX_PAYLOAD_BYTES {
                return Err(invalid_input("journal prefix payload exceeds 8 MiB"));
            }
            let frame_len = (RECORD_PREFIX_LEN as u64)
                .checked_add(RECORD_FIXED_BODY_LEN as u64)
                .and_then(|length| length.checked_add(record.payload.len() as u64))
                .ok_or_else(|| invalid_input("journal prefix size overflow"))?;
            prefix_len = prefix_len
                .checked_add(frame_len)
                .filter(|length| *length <= MAX_JOURNAL_BYTES)
                .ok_or_else(|| invalid_input("journal exceeds 256 MiB replay limit"))?;
        }
        let next_sequence = (keep_count as u64)
            .checked_add(1)
            .ok_or_else(|| invalid_input("journal sequence number overflow"))?;

        self.poisoned = true;
        self.file.set_len(prefix_len)?;
        self.file.sync_all()?;
        self.file.seek(SeekFrom::End(0))?;
        self.bytes_written = prefix_len;
        self.next_sequence = next_sequence;
        self.poisoned = false;
        Ok(())
    }

    /// Set a synthetic remaining append budget for app-level fail-closed tests.
    ///
    /// The on-disk journal remains valid and small; only the accounting position
    /// is advanced so tests can exercise the real append-limit branch without
    /// writing hundreds of megabytes of filler records.
    #[cfg(test)]
    pub(crate) fn set_remaining_capacity_for_test(&mut self, remaining: u64) {
        self.bytes_written = MAX_JOURNAL_BYTES.saturating_sub(remaining);
    }

    #[cfg(test)]
    pub(crate) fn set_next_sequence_for_test(&mut self, next_sequence: u64) {
        self.next_sequence = next_sequence;
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

fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for byte in *part {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
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

    // Older recovery assertions exercise Paint-only fixtures and use the
    // legacy field-style view. Keep that view test-local so production storage
    // remains the recursive LayerTree.
    impl std::ops::Deref for LayerNode {
        type Target = Layer;
        fn deref(&self) -> &Self::Target {
            match self {
                LayerNode::Paint(layer) => layer,
                LayerNode::Fill(_) | LayerNode::Group(_) => {
                    panic!("legacy test view requires Paint node")
                }
            }
        }
    }
    impl std::ops::DerefMut for LayerNode {
        fn deref_mut(&mut self) -> &mut Self::Target {
            match self {
                LayerNode::Paint(layer) => layer,
                LayerNode::Fill(_) | LayerNode::Group(_) => {
                    panic!("legacy test view requires Paint node")
                }
            }
        }
    }

    fn selected_root_index(tree: &LayerTree) -> usize {
        tree.roots
            .iter()
            .position(|node| node.id() == tree.selected_node_id)
            .unwrap_or(tree.roots.len())
    }

    #[test]
    fn incremental_crc_matches_contiguous_crc_without_allocating_joined_input() {
        let sequence = 0x1020_3040u64.to_le_bytes();
        let length = 9u32.to_le_bytes();
        let payload = b"journal!";
        let mut contiguous = Vec::new();
        contiguous.extend_from_slice(&sequence);
        contiguous.extend_from_slice(&length);
        contiguous.extend_from_slice(payload);
        assert_eq!(
            crc32_parts(&[&sequence, &length, payload]),
            crc32(&contiguous)
        );
        assert_eq!(crc32_parts(&[]), crc32(&[]));
    }
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

    fn event_record(sequence: u64, events: &[DocumentEvent]) -> Record {
        Record {
            sequence,
            payload: encode_event_batch_bounded(events).expect("encode event record"),
        }
    }

    fn import_events(set_id: u32, layer_id: u32) -> Vec<DocumentEvent> {
        vec![
            DocumentEvent::TextureImportBegin {
                set_id,
                layer_id,
                channel: Channel::BaseColor,
            },
            DocumentEvent::LayerCreate {
                set_id,
                layer_id,
                to_index: 0,
                name: "Imported Base Color".into(),
            },
            DocumentEvent::LayerImagePatch {
                set_id,
                layer_id,
                target: LayerImageTarget::Channel(Channel::BaseColor),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                bytes: vec![1, 2, 3, 4],
            },
            DocumentEvent::LayerSelection { set_id, layer_id },
            DocumentEvent::TextureImportCommit {
                set_id,
                layer_id,
                channel: Channel::BaseColor,
            },
        ]
    }

    fn records_from_events(events: &[DocumentEvent]) -> Vec<Record> {
        events
            .iter()
            .enumerate()
            .map(|(index, event)| event_record(index as u64 + 1, std::slice::from_ref(event)))
            .collect()
    }

    #[test]
    fn committed_record_count_keeps_complete_import_transactions() {
        let mut events = vec![DocumentEvent::LayerVisibility {
            set_id: 1,
            layer_id: 1,
            visible: false,
        }];
        events.extend(import_events(1, 2));
        events.push(DocumentEvent::LayerOpacity {
            set_id: 1,
            layer_id: 1,
            opacity: 0.5,
        });

        let records = records_from_events(&events);
        assert_eq!(committed_record_count(&records).unwrap(), records.len());

        let mut doc = Document::new(64);
        let before_layer_id = doc.selected_layer().unwrap().id;
        let before_selected = selected_root_index(&doc.layer_tree);
        let before_mask = doc.selected_layer().unwrap().mask.as_raw().to_vec();
        DocumentEvent::TextureImportBegin {
            set_id: 1,
            layer_id: 2,
            channel: Channel::BaseColor,
        }
        .apply(&mut doc)
        .unwrap();
        DocumentEvent::TextureImportCommit {
            set_id: 1,
            layer_id: 2,
            channel: Channel::BaseColor,
        }
        .apply(&mut doc)
        .unwrap();
        assert_eq!(
            doc.layer_tree.roots.len(),
            1,
            "transaction markers are replay no-ops"
        );
        assert_eq!(selected_root_index(&doc.layer_tree), before_selected);
        assert_eq!(doc.selected_layer().unwrap().id, before_layer_id);
        assert_eq!(
            doc.selected_layer().unwrap().mask.as_raw().as_slice(),
            before_mask.as_slice()
        );
    }

    #[test]
    fn committed_record_count_drops_only_incomplete_final_import() {
        let mut events = vec![DocumentEvent::LayerVisibility {
            set_id: 1,
            layer_id: 1,
            visible: false,
        }];
        events.extend(import_events(1, 2)[..4].iter().cloned());
        let records = records_from_events(&events);

        assert_eq!(committed_record_count(&records).unwrap(), 1);
    }

    #[test]
    fn committed_record_count_tracks_generated_layer_channel_transactions() {
        let events = [
            DocumentEvent::LayerChannelBegin {
                set_id: 1,
                layer_id: 2,
                channel: Channel::Occlusion,
            },
            DocumentEvent::LayerCreate {
                set_id: 1,
                layer_id: 2,
                to_index: 1,
                name: "Baked AO".into(),
            },
            DocumentEvent::LayerImagePatch {
                set_id: 1,
                layer_id: 2,
                target: LayerImageTarget::Channel(Channel::Occlusion),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                bytes: vec![255, 255, 255, 255],
            },
            DocumentEvent::LayerSelection {
                set_id: 1,
                layer_id: 2,
            },
            DocumentEvent::LayerChannelCommit {
                set_id: 1,
                layer_id: 2,
                channel: Channel::Occlusion,
            },
        ];
        let complete = records_from_events(&events);
        assert_eq!(committed_record_count(&complete).unwrap(), complete.len());

        let incomplete = records_from_events(&events[..4]);
        assert_eq!(committed_record_count(&incomplete).unwrap(), 0);

        let mismatched = [
            DocumentEvent::LayerChannelBegin {
                set_id: 1,
                layer_id: 2,
                channel: Channel::Occlusion,
            },
            DocumentEvent::TextureImportCommit {
                set_id: 1,
                layer_id: 2,
                channel: Channel::Occlusion,
            },
        ];
        assert!(committed_record_count(&records_from_events(&mismatched)).is_err());
    }

    fn snapshot_events(set_id: u32, layer_id: u32) -> Vec<DocumentEvent> {
        let mask = (0..64 * 64).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let roughness = (0..64 * 64 * 4)
            .map(|i| (i % 239) as u8)
            .collect::<Vec<_>>();
        let occlusion = (0..64 * 64 * 4)
            .map(|i| (255 - (i % 211)) as u8)
            .collect::<Vec<_>>();
        vec![
            DocumentEvent::LayerSnapshotBegin {
                set_id,
                layer_id,
                to_index: 1,
                selected_after_id: layer_id,
                metadata: LayerSnapshotMetadata {
                    name: "Recovered authored layer".into(),
                    visible: false,
                    opacity: 0.375,
                    blend: BlendMode::Multiply,
                    disabled_channels: vec![Channel::Metallic, Channel::Occlusion],
                    procedural_graph: Some(crate::procedural::ProceduralGraph::value_noise(
                        71, 9.5,
                    )),
                    extensions: serde_json::from_value(serde_json::json!({
                        "future_layer_property": {"version": 7, "opaque": [1, 2, 3]}
                    }))
                    .unwrap(),
                },
                channels: vec![Channel::Roughness, Channel::Occlusion],
            },
            DocumentEvent::LayerSnapshotChunk {
                set_id,
                layer_id,
                target: LayerImageTarget::Mask,
                offset: 0,
                bytes: mask,
            },
            DocumentEvent::LayerSnapshotChunk {
                set_id,
                layer_id,
                target: LayerImageTarget::Channel(Channel::Roughness),
                offset: 0,
                bytes: roughness,
            },
            DocumentEvent::LayerSnapshotChunk {
                set_id,
                layer_id,
                target: LayerImageTarget::Channel(Channel::Occlusion),
                offset: 0,
                bytes: occlusion,
            },
            DocumentEvent::LayerSnapshotCommit {
                set_id,
                layer_id,
                selected_after_id: layer_id,
            },
        ]
    }

    #[test]
    fn layer_snapshot_restores_full_layer_state_and_forward_extensions() {
        let mut doc = Document::new(64);
        doc.layer_tree.next_node_id = 3; // ID 2 was allocated and then removed.
        let events = snapshot_events(1, 2);
        let encoded = events[1].encode_bounded().unwrap();
        assert!(std::str::from_utf8(&encoded)
            .unwrap()
            .contains("\"bytes\":\""));
        assert_eq!(DocumentEvent::decode_bounded(&encoded).unwrap(), events[1]);

        for event in &events {
            event.apply(&mut doc).unwrap();
        }
        let layer = doc
            .layer_tree
            .roots
            .iter()
            .find(|layer| layer.id == 2)
            .unwrap();
        assert_eq!(layer.name, "Recovered authored layer");
        assert!(!layer.visible);
        assert_eq!(layer.opacity, 0.375);
        assert_eq!(layer.blend, BlendMode::Multiply);
        assert_eq!(
            layer.procedural_graph,
            Some(crate::procedural::ProceduralGraph::value_noise(71, 9.5))
        );
        assert!(layer.disabled_channels.contains(&Channel::Metallic));
        assert!(layer.disabled_channels.contains(&Channel::Occlusion));
        assert_eq!(layer.mask.as_raw(), events_mask(&events));
        assert_eq!(
            layer.pixels[&Channel::Roughness].as_raw(),
            events_channel(&events, Channel::Roughness)
        );
        assert_eq!(
            layer.pixels[&Channel::Occlusion].as_raw(),
            events_channel(&events, Channel::Occlusion)
        );
        assert_eq!(
            layer.extensions["future_layer_property"],
            serde_json::json!({"version": 7, "opaque": [1, 2, 3]})
        );
        assert_eq!(doc.selected_layer().unwrap().id, 2);
    }

    #[test]
    fn snapshot_chunk_codec_accepts_legacy_arrays_and_caps_raw_bytes() {
        let legacy = br#"{"event":"layer_snapshot_chunk","set_id":1,"layer_id":2,"target":{"kind":"mask"},"offset":0,"bytes":[1,2,3]}"#;
        assert_eq!(
            DocumentEvent::decode_bounded(legacy).unwrap(),
            DocumentEvent::LayerSnapshotChunk {
                set_id: 1,
                layer_id: 2,
                target: LayerImageTarget::Mask,
                offset: 0,
                bytes: vec![1, 2, 3],
            }
        );
        let too_large = DocumentEvent::LayerSnapshotChunk {
            set_id: 1,
            layer_id: 2,
            target: LayerImageTarget::Mask,
            offset: 0,
            bytes: vec![0; MAX_SNAPSHOT_CHUNK_BYTES + 1],
        };
        assert!(too_large.encode_bounded().is_err());
        assert!(too_large.apply(&mut Document::new(64)).is_err());
    }

    fn events_mask(events: &[DocumentEvent]) -> &[u8] {
        match &events[1] {
            DocumentEvent::LayerSnapshotChunk { bytes, .. } => bytes,
            _ => unreachable!(),
        }
    }

    fn events_channel(events: &[DocumentEvent], channel: Channel) -> &[u8] {
        events
            .iter()
            .find_map(|event| match event {
                DocumentEvent::LayerSnapshotChunk {
                    target: LayerImageTarget::Channel(target),
                    bytes,
                    ..
                } if *target == channel => Some(bytes.as_slice()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn snapshot_scanner_truncates_incomplete_final_transaction_and_rejects_bad_order() {
        let complete = snapshot_events(1, 2);
        let records = records_from_events(&complete);
        assert_eq!(committed_record_count(&records).unwrap(), records.len());

        let prefix = DocumentEvent::LayerVisibility {
            set_id: 1,
            layer_id: 1,
            visible: false,
        };
        let mut incomplete = vec![prefix];
        incomplete.extend(complete[..3].iter().cloned());
        assert_eq!(
            committed_record_count(&records_from_events(&incomplete)).unwrap(),
            1
        );

        let wrong_target = vec![complete[0].clone(), complete[2].clone()];
        assert!(committed_record_count(&records_from_events(&wrong_target)).is_err());

        let mut wrong_offset = complete[..2].to_vec();
        if let DocumentEvent::LayerSnapshotChunk { offset, .. } = &mut wrong_offset[1] {
            *offset = 1;
        }
        assert!(committed_record_count(&records_from_events(&wrong_offset)).is_err());

        let mut wrong_commit = complete.clone();
        wrong_commit[4] = DocumentEvent::LayerSnapshotCommit {
            set_id: 1,
            layer_id: 2,
            selected_after_id: 1,
        };
        assert!(committed_record_count(&records_from_events(&wrong_commit)).is_err());
    }

    #[test]
    fn snapshot_apply_rejects_invalid_ranges_and_budget_without_mutation() {
        let mut doc = Document::new(64);
        doc.layer_tree.next_node_id = 3;
        let begin = snapshot_events(1, 2).remove(0);
        begin.apply(&mut doc).unwrap();
        let baseline_mask = doc.layer_tree.roots[1].mask.clone();
        let invalid_chunk = DocumentEvent::LayerSnapshotChunk {
            set_id: 1,
            layer_id: 2,
            target: LayerImageTarget::Channel(Channel::Roughness),
            offset: 64 * 64 * 4,
            bytes: vec![7],
        };
        assert!(invalid_chunk.apply(&mut doc).is_err());
        assert!(doc.layer_tree.roots[1].pixels.is_empty());
        assert_eq!(
            doc.layer_tree.roots[1].mask.as_raw(),
            baseline_mask.as_raw()
        );

        let mut over_budget = Document::new(MAX_SIZE);
        over_budget.layer_tree.next_node_id = 3;
        let overlarge = DocumentEvent::LayerSnapshotBegin {
            set_id: 1,
            layer_id: 2,
            to_index: 1,
            selected_after_id: 2,
            metadata: LayerSnapshotMetadata {
                name: "Too much".into(),
                visible: true,
                opacity: 1.0,
                blend: BlendMode::Normal,
                disabled_channels: vec![],
                procedural_graph: None,
                extensions: Default::default(),
            },
            channels: Channel::ALL.to_vec(),
        };
        let old_len = over_budget.layer_tree.roots.len();
        assert!(overlarge.apply(&mut over_budget).is_err());
        assert_eq!(over_budget.layer_tree.roots.len(), old_len);
        assert_eq!(over_budget.layer_tree.next_node_id, 3);
    }

    fn mesh_replace_events(
        mesh_bytes: &[u8],
        transaction_id: Uuid,
        material_bindings: Vec<Option<u32>>,
        digest: [u8; 32],
    ) -> Vec<DocumentEvent> {
        let split = mesh_bytes.len().min(mesh_bytes.len() / 2).max(1);
        let mut events = vec![DocumentEvent::MeshReplaceBegin {
            transaction_id,
            encoded_len: mesh_bytes.len() as u64,
            sha256: digest,
            material_bindings,
        }];
        for (offset, bytes) in [(0, &mesh_bytes[..split]), (split, &mesh_bytes[split..])] {
            if !bytes.is_empty() {
                events.push(DocumentEvent::MeshReplaceChunk {
                    transaction_id,
                    offset: offset as u64,
                    bytes: bytes.to_vec(),
                });
            }
        }
        events.push(DocumentEvent::MeshReplaceCommit { transaction_id });
        events
    }

    fn two_material_mesh_json() -> Vec<u8> {
        let mut mesh = crate::mesh::Mesh::demo_cube();
        mesh.name = "Recovery mesh".into();
        mesh.materials = vec!["First".into(), "Second".into()];
        serde_json::to_vec(&mesh).unwrap()
    }

    #[test]
    fn mesh_replace_codec_and_replayer_commit_complete_multi_record_transaction() {
        let mut doc = Document::new(64);
        let original_name = doc.mesh.name.clone();
        let encoded_mesh = two_material_mesh_json();
        let digest: [u8; 32] = Sha256::digest(&encoded_mesh).into();
        let tx = Uuid::new_v4();
        let events = mesh_replace_events(&encoded_mesh, tx, vec![Some(1), Some(1)], digest);
        let first_chunk = events[1].encode_bounded().unwrap();
        assert!(std::str::from_utf8(&first_chunk)
            .unwrap()
            .contains("\"bytes\":\""));
        assert_eq!(
            DocumentEvent::decode_bounded(&first_chunk).unwrap(),
            events[1]
        );
        assert_eq!(
            committed_record_count(&records_from_events(&events)).unwrap(),
            events.len()
        );

        let mut replayer = MeshRecoveryReplayer::new();
        for event in events {
            replayer.apply_event(&mut doc, event).unwrap();
        }
        replayer.finish().unwrap();
        assert_ne!(doc.mesh.name, original_name);
        assert_eq!(doc.mesh.name, "Recovery mesh");
        assert_eq!(doc.mesh.material_count(), 2);
        assert_eq!(doc.material_bindings, vec![Some(1), Some(1)]);
    }

    #[test]
    fn mesh_replace_scanner_truncates_incomplete_tail_and_rejects_malformed_sequence() {
        let mesh = two_material_mesh_json();
        let digest: [u8; 32] = Sha256::digest(&mesh).into();
        let tx = Uuid::new_v4();
        let complete = mesh_replace_events(&mesh, tx, vec![Some(1), Some(1)], digest);
        let prefix = DocumentEvent::LayerVisibility {
            set_id: 1,
            layer_id: 1,
            visible: false,
        };
        let mut incomplete = vec![prefix];
        incomplete.extend(complete[..2].iter().cloned());
        assert_eq!(
            committed_record_count(&records_from_events(&incomplete)).unwrap(),
            1
        );

        let mut wrong_offset = complete.clone();
        if let DocumentEvent::MeshReplaceChunk { offset, .. } = &mut wrong_offset[2] {
            *offset += 1;
        }
        assert!(committed_record_count(&records_from_events(&wrong_offset)).is_err());

        let mut wrong_id = complete.clone();
        wrong_id[2] = DocumentEvent::MeshReplaceChunk {
            transaction_id: Uuid::new_v4(),
            offset: 0,
            bytes: mesh[..mesh.len() / 2].to_vec(),
        };
        assert!(committed_record_count(&records_from_events(&wrong_id)).is_err());

        let mut wrong_hash = complete;
        if let DocumentEvent::MeshReplaceBegin { sha256, .. } = &mut wrong_hash[0] {
            *sha256 = [0x99; 32];
        }
        assert!(committed_record_count(&records_from_events(&wrong_hash)).is_err());
    }

    #[test]
    fn mesh_replace_replayer_rejects_hash_mesh_and_binding_errors_without_mutation() {
        let mut doc = Document::new(64);
        let original_mesh_name = doc.mesh.name.clone();
        let original_bindings = doc.material_bindings.clone();

        let valid_mesh = two_material_mesh_json();
        let tx = Uuid::new_v4();
        let wrong_hash = mesh_replace_events(&valid_mesh, tx, vec![Some(1), Some(1)], [0; 32]);
        let mut replayer = MeshRecoveryReplayer::new();
        for event in wrong_hash {
            if let Err(error) = replayer.apply_event(&mut doc, event) {
                assert!(error.to_string().contains("SHA-256"));
                break;
            }
        }
        assert_eq!(doc.mesh.name, original_mesh_name);
        assert_eq!(doc.material_bindings, original_bindings);

        let invalid_mesh = br#"{"name":"broken","vertices":[],"indices":[]}"#;
        let digest: [u8; 32] = Sha256::digest(invalid_mesh).into();
        let invalid_events =
            mesh_replace_events(invalid_mesh, Uuid::new_v4(), vec![Some(1)], digest);
        let mut replayer = MeshRecoveryReplayer::new();
        let error = invalid_events
            .into_iter()
            .try_for_each(|event| replayer.apply_event(&mut doc, event))
            .unwrap_err();
        assert!(error.to_string().contains("empty"));
        assert_eq!(doc.mesh.name, original_mesh_name);

        let digest: [u8; 32] = Sha256::digest(&valid_mesh).into();
        let wrong_binding_count =
            mesh_replace_events(&valid_mesh, Uuid::new_v4(), vec![Some(1)], digest);
        let mut replayer = MeshRecoveryReplayer::new();
        let error = wrong_binding_count
            .into_iter()
            .try_for_each(|event| replayer.apply_event(&mut doc, event))
            .unwrap_err();
        assert!(error.to_string().contains("binding count"));
        assert_eq!(doc.mesh.name, original_mesh_name);
        assert_eq!(doc.material_bindings, original_bindings);

        let unknown_set = DocumentEvent::MeshReplaceBegin {
            transaction_id: Uuid::new_v4(),
            encoded_len: valid_mesh.len() as u64,
            sha256: digest,
            material_bindings: vec![Some(99), Some(99)],
        };
        assert!(unknown_set.apply(&mut doc).is_err());
        assert!(MeshRecoveryReplayer::new()
            .apply_event(&mut doc, unknown_set)
            .is_err());
        assert_eq!(doc.mesh.name, original_mesh_name);
        assert_eq!(doc.material_bindings, original_bindings);
    }

    #[test]
    fn committed_record_count_rejects_malformed_import_marker_sequences() {
        let begin = DocumentEvent::TextureImportBegin {
            set_id: 1,
            layer_id: 2,
            channel: Channel::BaseColor,
        };
        let nested = DocumentEvent::TextureImportBegin {
            set_id: 1,
            layer_id: 3,
            channel: Channel::BaseColor,
        };
        let mismatched_commit = DocumentEvent::TextureImportCommit {
            set_id: 1,
            layer_id: 3,
            channel: Channel::BaseColor,
        };
        let unmatched_commit = DocumentEvent::TextureImportCommit {
            set_id: 1,
            layer_id: 2,
            channel: Channel::BaseColor,
        };
        let create = DocumentEvent::LayerCreate {
            set_id: 1,
            layer_id: 2,
            to_index: 0,
            name: "Imported".into(),
        };
        let wrong_channel_patch = DocumentEvent::LayerImagePatch {
            set_id: 1,
            layer_id: 2,
            target: LayerImageTarget::Channel(Channel::Roughness),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bytes: vec![1, 2, 3, 4],
        };
        let cases = [
            vec![unmatched_commit],
            vec![begin.clone(), nested],
            vec![begin.clone(), mismatched_commit],
            vec![begin.clone(), create.clone(), wrong_channel_patch],
            vec![
                begin.clone(),
                DocumentEvent::LayerSelection {
                    set_id: 1,
                    layer_id: 2,
                },
            ],
            vec![
                begin,
                DocumentEvent::LayerCreate {
                    set_id: 1,
                    layer_id: 9,
                    to_index: 0,
                    name: "Wrong layer".into(),
                },
            ],
        ];

        for events in cases {
            assert!(
                committed_record_count(&records_from_events(&events)).is_err(),
                "malformed events must fail closed: {events:?}"
            );
        }
    }

    #[test]
    fn truncate_records_syncs_prefix_and_append_continues_sequence() {
        let path = TempPath::new("truncate-import-transaction");
        let import = import_events(1, 2);
        let events = [
            DocumentEvent::LayerVisibility {
                set_id: 1,
                layer_id: 1,
                visible: false,
            },
            import[0].clone(),
            import[1].clone(),
            import[2].clone(),
        ];
        let payloads = events
            .iter()
            .map(|event| encode_event_batch_bounded(std::slice::from_ref(event)).unwrap())
            .collect::<Vec<_>>();
        let mut journal = Journal::create(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        for payload in &payloads {
            journal.append(payload).unwrap();
        }
        let original_len = fs::metadata(&path.0).unwrap().len();
        let (_, scanned) = Journal::open(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        let keep_count = committed_record_count(&scanned).unwrap();
        assert_eq!(keep_count, 1);

        journal.truncate_records(&scanned, keep_count).unwrap();
        let prefix_len = HEADER_LEN as u64
            + RECORD_PREFIX_LEN as u64
            + RECORD_FIXED_BODY_LEN as u64
            + payloads[0].len() as u64;
        assert_eq!(fs::metadata(&path.0).unwrap().len(), prefix_len);
        assert!(prefix_len < original_len);

        let appended = encode_event_batch_bounded(&[DocumentEvent::LayerVisibility {
            set_id: 1,
            layer_id: 1,
            visible: true,
        }])
        .unwrap();
        assert_eq!(journal.append(&appended).unwrap(), 2);
        let records = scan(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].sequence, 2);
        assert_eq!(committed_record_count(&records).unwrap(), 2);
    }

    #[test]
    fn image_patch_event_roundtrips_and_replays_channel_and_mask_bytes() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
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
        let image = &doc.selected_layer().unwrap().pixels[&Channel::BaseColor];
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
        assert_eq!(doc.selected_layer().unwrap().mask.get_pixel(8, 9).0, [3]);
        assert_eq!(doc.selected_layer().unwrap().mask.get_pixel(9, 10).0, [6]);
    }

    #[test]
    fn channel_remove_replay_removes_only_existing_channel_buffer() {
        let mut doc = Document::new(64);
        let set_id = doc.active_set_id;
        let layer_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Roughness, 64)
            .put_pixel(7, 9, image::Rgba([19, 0, 0, 255]));

        let event = DocumentEvent::LayerChannelRemove {
            set_id,
            layer_id,
            channel: Channel::Roughness,
        };
        let encoded = event.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();
        assert!(!doc
            .selected_layer()
            .unwrap()
            .pixels
            .contains_key(&Channel::Roughness));
        assert_eq!(doc.selected_layer().unwrap().mask.dimensions(), (64, 64));
    }

    #[test]
    fn channel_remove_rejects_missing_targets_without_mutation() {
        let mut doc = Document::new(64);
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_bytes = doc.decoded_bytes();
        let before_next_id = doc.layer_tree.next_node_id;
        let invalid = [
            DocumentEvent::LayerChannelRemove {
                set_id: 0,
                layer_id: 1,
                channel: Channel::Roughness,
            },
            DocumentEvent::LayerChannelRemove {
                set_id: doc.active_set_id,
                layer_id: 99,
                channel: Channel::Roughness,
            },
            DocumentEvent::LayerChannelRemove {
                set_id: doc.active_set_id,
                layer_id: 1,
                channel: Channel::Roughness,
            },
        ];
        for event in invalid {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before_ids
            );
            assert_eq!(selected_root_index(&doc.layer_tree), 0);
            assert_eq!(doc.layer_tree.next_node_id, before_next_id);
            assert_eq!(doc.decoded_bytes(), before_bytes);
        }
    }

    #[test]
    fn structural_events_roundtrip_and_replay_exact_layer_state() {
        let mut doc = Document::new(64);
        let first_id = doc.layer_tree.roots[0].id;
        doc.add_layer().unwrap();
        let second_id = doc.layer_tree.roots[1].id;
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
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            [second_id, first_id]
        );
        assert_eq!(doc.layer_tree.roots[1].name, "Paint • details");
        assert_eq!(doc.layer_tree.roots[1].opacity, 0.375);
        assert!(!doc.layer_tree.roots[0].visible);
        assert!(doc.layer_tree.roots[0]
            .disabled_channels
            .contains(&Channel::Metallic));
        assert_eq!(
            selected_root_index(&doc.layer_tree),
            0,
            "reorder preserves selection by layer identity"
        );
    }

    #[test]
    fn layer_create_codec_and_replay_preserve_active_selection_by_id() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        doc.layer_tree.select(doc.layer_tree.roots[0].id()).unwrap();
        let selected_id = doc.layer_tree.roots[selected_root_index(&doc.layer_tree)].id;
        let event = DocumentEvent::LayerCreate {
            set_id: doc.active_set_id,
            layer_id: doc.layer_tree.next_node_id,
            to_index: 0,
            name: "Paint Layer 3".into(),
        };
        let encoded = event.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        assert_eq!(doc.layer_tree.roots[0].name, "Paint Layer 3");
        assert_eq!(doc.layer_tree.roots[0].id, 3);
        assert_eq!(doc.selected_layer().unwrap().id, selected_id);
        assert_eq!(selected_root_index(&doc.layer_tree), 1);
        assert_eq!(doc.layer_tree.next_node_id, 4);
        assert_eq!(doc.layer_tree.roots[0].mask.dimensions(), (64, 64));
        assert!(doc.layer_tree.roots[0]
            .mask
            .pixels()
            .all(|pixel| pixel.0 == [255]));
        assert!(doc.layer_tree.roots[0].pixels.is_empty());
    }

    #[test]
    fn layer_create_replay_supports_inactive_texture_sets_only() {
        let mut doc = Document::new(64);
        let set_id = doc.add_texture_set("Recovery Set").unwrap();
        doc.add_layer().unwrap();
        let selected_id = doc.selected_layer().unwrap().id;
        let inactive_next_id = doc.layer_tree.next_node_id;
        doc.activate_texture_set(1).unwrap();
        let active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();

        DocumentEvent::LayerCreate {
            set_id,
            layer_id: inactive_next_id,
            to_index: 0,
            name: "Inactive Added".into(),
        }
        .apply(&mut doc)
        .unwrap();

        let inactive = doc
            .texture_sets
            .iter()
            .find(|set| set.id == set_id)
            .unwrap();
        assert_eq!(inactive.layer_tree.roots[0].id, inactive_next_id);
        assert_eq!(inactive.layer_tree.roots[0].name, "Inactive Added");
        assert_eq!(
            inactive.layer_tree.roots[selected_root_index(&inactive.layer_tree)].id,
            selected_id
        );
        assert_eq!(selected_root_index(&inactive.layer_tree), 2);
        assert_eq!(inactive.layer_tree.next_node_id, inactive_next_id + 1);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            active_ids
        );
        assert_eq!(doc.layer_tree.next_node_id, 2);
    }

    #[test]
    fn layer_create_rejects_invalid_input_without_mutation() {
        let mut doc = Document::new(64);
        let valid_next_id = doc.layer_tree.next_node_id;
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_decoded_bytes = doc.decoded_bytes();
        let bad_events = [
            DocumentEvent::LayerCreate {
                set_id: 0,
                layer_id: valid_next_id,
                to_index: 1,
                name: "Bad set".into(),
            },
            DocumentEvent::LayerCreate {
                set_id: doc.active_set_id + 100,
                layer_id: valid_next_id,
                to_index: 1,
                name: "Missing set".into(),
            },
            DocumentEvent::LayerCreate {
                set_id: doc.active_set_id,
                layer_id: 0,
                to_index: 1,
                name: "Zero ID".into(),
            },
            DocumentEvent::LayerCreate {
                set_id: doc.active_set_id,
                layer_id: valid_next_id + 1,
                to_index: 1,
                name: "Skipped ID".into(),
            },
            DocumentEvent::LayerCreate {
                set_id: doc.active_set_id,
                layer_id: valid_next_id,
                to_index: 2,
                name: "Bad index".into(),
            },
            DocumentEvent::LayerCreate {
                set_id: doc.active_set_id,
                layer_id: valid_next_id,
                to_index: 1,
                name: String::new(),
            },
            DocumentEvent::LayerCreate {
                set_id: doc.active_set_id,
                layer_id: valid_next_id,
                to_index: 1,
                name: "x".repeat(1025),
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before_ids
            );
            assert_eq!(selected_root_index(&doc.layer_tree), 0);
            assert_eq!(doc.layer_tree.next_node_id, valid_next_id);
            assert_eq!(doc.decoded_bytes(), before_decoded_bytes);
        }

        let mut duplicate_id = Document::new(64);
        duplicate_id.layer_tree.roots[0].id = valid_next_id;
        duplicate_id.layer_tree.next_node_id = valid_next_id;
        let duplicate_before = duplicate_id.decoded_bytes();
        assert!(DocumentEvent::LayerCreate {
            set_id: duplicate_id.active_set_id,
            layer_id: valid_next_id,
            to_index: 1,
            name: "Duplicate ID".into(),
        }
        .apply(&mut duplicate_id)
        .is_err());
        assert_eq!(duplicate_id.layer_tree.roots.len(), 1);
        assert_eq!(duplicate_id.decoded_bytes(), duplicate_before);
    }

    #[test]
    fn layer_create_rejects_layer_limit_and_budget_overflow() {
        let mut full = Document::new(64);
        while full.layer_tree.roots.len() < MAX_LAYERS {
            full.add_layer().unwrap();
        }
        let full_next_id = full.layer_tree.next_node_id;
        assert!(DocumentEvent::LayerCreate {
            set_id: full.active_set_id,
            layer_id: full_next_id,
            to_index: full.layer_tree.roots.len(),
            name: "Over limit".into(),
        }
        .apply(&mut full)
        .is_err());
        assert_eq!(full.layer_tree.roots.len(), MAX_LAYERS);
        assert_eq!(full.layer_tree.next_node_id, full_next_id);

        assert!(layer_allocation_fits(MAX_DECODED_BYTES - 1, 1));
        assert!(!layer_allocation_fits(MAX_DECODED_BYTES, 1));
        assert!(!layer_allocation_fits(u64::MAX, 1));

        let mut exhausted = Document::new(64);
        exhausted.layer_tree.next_node_id = u32::MAX;
        let before_ids = exhausted
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        assert!(DocumentEvent::LayerCreate {
            set_id: exhausted.active_set_id,
            layer_id: u32::MAX,
            to_index: 1,
            name: "Counter overflow".into(),
        }
        .apply(&mut exhausted)
        .is_err());
        assert_eq!(
            exhausted
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(exhausted.layer_tree.next_node_id, u32::MAX);

        let mut invalid_size = Document::new(64);
        invalid_size.size = MAX_SIZE + 1;
        let before_ids = invalid_size
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        assert!(DocumentEvent::LayerCreate {
            set_id: invalid_size.active_set_id,
            layer_id: invalid_size.layer_tree.next_node_id,
            to_index: 1,
            name: "Invalid size".into(),
        }
        .apply(&mut invalid_size)
        .is_err());
        assert_eq!(
            invalid_size
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
    }

    #[test]
    fn layer_reinsert_restores_blank_and_duplicate_without_reusing_id_counter() {
        let mut blank_doc = Document::new(64);
        blank_doc.add_layer().unwrap();
        let blank_id = blank_doc.selected_layer().unwrap().id;
        let next_id = blank_doc.layer_tree.next_node_id;
        DocumentEvent::LayerRemove {
            set_id: blank_doc.active_set_id,
            layer_id: blank_id,
            selected_after_id: blank_doc.layer_tree.roots[0].id,
        }
        .apply(&mut blank_doc)
        .unwrap();
        DocumentEvent::LayerReinsert {
            set_id: blank_doc.active_set_id,
            layer_id: blank_id,
            to_index: 1,
            name: "Restored blank".into(),
            source_layer_id: None,
        }
        .apply(&mut blank_doc)
        .unwrap();
        assert_eq!(blank_doc.layer_tree.roots[1].id, blank_id);
        assert_eq!(blank_doc.layer_tree.roots[1].name, "Restored blank");
        assert_eq!(blank_doc.layer_tree.next_node_id, next_id);
        assert_eq!(
            blank_doc.selected_layer().unwrap().id,
            blank_doc.layer_tree.roots[0].id
        );

        let mut duplicate_doc = Document::new(64);
        duplicate_doc.selected_layer_mut().unwrap().name = "Source".into();
        duplicate_doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Roughness, 64)
            .put_pixel(2, 3, image::Rgba([87, 0, 0, 255]));
        duplicate_doc
            .selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(4, 5, image::Luma([62]));
        duplicate_doc.add_layer().unwrap();
        let duplicate_id = duplicate_doc.selected_layer().unwrap().id;
        let duplicate_next_id = duplicate_doc.layer_tree.next_node_id;
        DocumentEvent::LayerRemove {
            set_id: duplicate_doc.active_set_id,
            layer_id: duplicate_id,
            selected_after_id: duplicate_doc.layer_tree.roots[0].id,
        }
        .apply(&mut duplicate_doc)
        .unwrap();
        DocumentEvent::LayerReinsert {
            set_id: duplicate_doc.active_set_id,
            layer_id: duplicate_id,
            to_index: 1,
            name: "Source copy".into(),
            source_layer_id: Some(duplicate_doc.layer_tree.roots[0].id),
        }
        .apply(&mut duplicate_doc)
        .unwrap();
        let duplicate = &duplicate_doc.layer_tree.roots[1];
        assert_eq!(duplicate.id, duplicate_id);
        assert_eq!(duplicate.name, "Source copy");
        assert_eq!(
            duplicate.pixels[&Channel::Roughness].get_pixel(2, 3),
            &image::Rgba([87, 0, 0, 255])
        );
        assert_eq!(duplicate.mask.get_pixel(4, 5), &image::Luma([62]));
        assert_eq!(duplicate_doc.layer_tree.next_node_id, duplicate_next_id);
    }

    #[test]
    fn layer_reinsert_rejects_unallocated_ids_and_bad_sources_without_mutation() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let removed_id = doc.selected_layer().unwrap().id;
        DocumentEvent::LayerRemove {
            set_id: doc.active_set_id,
            layer_id: removed_id,
            selected_after_id: doc.layer_tree.roots[0].id,
        }
        .apply(&mut doc)
        .unwrap();
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_next = doc.layer_tree.next_node_id;
        let before_bytes = doc.decoded_bytes();
        for source_layer_id in [Some(999), None] {
            let event = DocumentEvent::LayerReinsert {
                set_id: doc.active_set_id,
                layer_id: before_next,
                to_index: 1,
                name: "Invalid reinsert".into(),
                source_layer_id,
            };
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before_ids
            );
            assert_eq!(doc.layer_tree.next_node_id, before_next);
            assert_eq!(doc.decoded_bytes(), before_bytes);
        }
    }

    #[test]
    fn layer_remove_codec_and_replay_discards_full_active_layer_state() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let removed_id = doc.selected_layer().unwrap().id;
        let selected_after_id = doc.layer_tree.roots[0].id;
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, size)
            .put_pixel(2, 3, image::Rgba([12, 34, 56, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(4, 5, image::Luma([73]));
        doc.selected_layer_mut()
            .unwrap()
            .extensions
            .insert("future".into(), serde_json::json!({ "keep": true }));
        doc.selected_layer_mut().unwrap().name = "Authored, nonblank layer".into();
        doc.selected_layer_mut().unwrap().visible = false;
        doc.selected_layer_mut().unwrap().opacity = 0.5;
        doc.selected_layer_mut().unwrap().blend = BlendMode::Multiply;
        doc.selected_layer_mut()
            .unwrap()
            .disabled_channels
            .insert(Channel::Metallic);
        let next_layer_id = doc.layer_tree.next_node_id;

        let event = DocumentEvent::LayerRemove {
            set_id: doc.active_set_id,
            layer_id: removed_id,
            selected_after_id,
        };
        let encoded = event.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(doc.layer_tree.roots[0].id, selected_after_id);
        assert_eq!(doc.selected_layer().unwrap().id, selected_after_id);
        assert_eq!(selected_root_index(&doc.layer_tree), 0);
        assert_eq!(doc.layer_tree.next_node_id, next_layer_id);
    }

    #[test]
    fn layer_remove_replay_supports_inactive_texture_sets() {
        let mut doc = Document::new(64);
        let inactive_set_id = doc.add_texture_set("Recovery Set").unwrap();
        doc.add_layer().unwrap();
        let removed_id = doc.selected_layer().unwrap().id;
        let selected_after_id = doc.layer_tree.roots[0].id;
        let inactive_next_id = doc.layer_tree.next_node_id;
        doc.activate_texture_set(1).unwrap();
        let active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let active_next_id = doc.layer_tree.next_node_id;

        DocumentEvent::LayerRemove {
            set_id: inactive_set_id,
            layer_id: removed_id,
            selected_after_id,
        }
        .apply(&mut doc)
        .unwrap();

        let inactive = doc
            .texture_sets
            .iter()
            .find(|set| set.id == inactive_set_id)
            .unwrap();
        assert_eq!(inactive.layer_tree.roots.len(), 1);
        assert_eq!(inactive.layer_tree.roots[0].id, selected_after_id);
        assert_eq!(selected_root_index(&inactive.layer_tree), 0);
        assert_eq!(inactive.layer_tree.next_node_id, inactive_next_id);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            active_ids
        );
        assert_eq!(doc.layer_tree.next_node_id, active_next_id);
    }

    #[test]
    fn layer_remove_rejects_invalid_inputs_without_mutation() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let selected_id = doc.selected_layer().unwrap().id;
        let first_id = doc.layer_tree.roots[0].id;
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Roughness, size)
            .put_pixel(1, 1, image::Rgba([99, 0, 0, 255]));
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_selected = selected_root_index(&doc.layer_tree);
        let before_next_id = doc.layer_tree.next_node_id;
        let before_decoded_bytes = doc.decoded_bytes();
        let before_pixel = doc.selected_layer().unwrap().pixels[&Channel::Roughness].clone();
        let bad_events = [
            DocumentEvent::LayerRemove {
                set_id: 0,
                layer_id: selected_id,
                selected_after_id: first_id,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id + 100,
                layer_id: selected_id,
                selected_after_id: first_id,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id,
                layer_id: 0,
                selected_after_id: first_id,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id,
                layer_id: selected_id,
                selected_after_id: 0,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id,
                layer_id: first_id,
                selected_after_id: selected_id,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id,
                layer_id: selected_id,
                selected_after_id: selected_id,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id,
                layer_id: selected_id,
                selected_after_id: first_id + 100,
            },
            DocumentEvent::LayerRemove {
                set_id: doc.active_set_id,
                layer_id: selected_id + 100,
                selected_after_id: first_id,
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before_ids
            );
            assert_eq!(selected_root_index(&doc.layer_tree), before_selected);
            assert_eq!(doc.layer_tree.next_node_id, before_next_id);
            assert_eq!(doc.decoded_bytes(), before_decoded_bytes);
            assert_eq!(
                doc.selected_layer().unwrap().pixels[&Channel::Roughness],
                before_pixel
            );
        }

        let mut only_layer = Document::new(64);
        let only_id = only_layer.selected_layer().unwrap().id;
        let before_next_id = only_layer.layer_tree.next_node_id;
        assert!(DocumentEvent::LayerRemove {
            set_id: only_layer.active_set_id,
            layer_id: only_id,
            selected_after_id: only_id + 1,
        }
        .apply(&mut only_layer)
        .is_err());
        assert_eq!(only_layer.layer_tree.roots.len(), 1);
        assert_eq!(selected_root_index(&only_layer.layer_tree), 0);
        assert_eq!(only_layer.layer_tree.next_node_id, before_next_id);

        let mut invalid_selection = Document::new(64);
        invalid_selection.add_layer().unwrap();
        invalid_selection.layer_tree.selected_node_id = u32::MAX;
        let before_ids = invalid_selection
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_next_id = invalid_selection.layer_tree.next_node_id;
        assert!(DocumentEvent::LayerRemove {
            set_id: invalid_selection.active_set_id,
            layer_id: invalid_selection.layer_tree.roots[1].id,
            selected_after_id: invalid_selection.layer_tree.roots[0].id,
        }
        .apply(&mut invalid_selection)
        .is_err());
        assert_eq!(
            invalid_selection
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(selected_root_index(&invalid_selection.layer_tree), 2);
        assert_eq!(invalid_selection.layer_tree.next_node_id, before_next_id);
    }

    #[test]
    fn texture_set_rename_codec_and_active_replay_trim_name() {
        let mut doc = Document::new(64);
        let original_layers = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let original_selected = selected_root_index(&doc.layer_tree);
        let original_next_layer_id = doc.layer_tree.next_node_id;
        let original_bindings = doc.material_bindings.clone();
        let exact_limit_name = format!("  {}\t", "é".repeat(64));
        let event = DocumentEvent::TextureSetRename {
            set_id: doc.active_set_id,
            name: exact_limit_name,
        };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        assert_eq!(doc.active_set_name, "é".repeat(64));
        assert_eq!(doc.active_set_name.len(), 128);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            original_layers
        );
        assert_eq!(selected_root_index(&doc.layer_tree), original_selected);
        assert_eq!(doc.layer_tree.next_node_id, original_next_layer_id);
        assert_eq!(doc.material_bindings, original_bindings);
    }

    #[test]
    fn texture_set_rename_replay_supports_inactive_sets_only() {
        let mut doc = Document::new(64);
        let inactive_id = doc.add_texture_set("Old Name").unwrap();
        let inactive_layer_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let inactive_selected = selected_root_index(&doc.layer_tree);
        let inactive_next_id = doc.layer_tree.next_node_id;
        let active_id = 1;
        doc.activate_texture_set(active_id).unwrap();
        let active_name = doc.active_set_name.clone();
        let active_layers = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();

        DocumentEvent::TextureSetRename {
            set_id: inactive_id,
            name: "  Renamed inactive  ".into(),
        }
        .apply(&mut doc)
        .unwrap();

        let inactive = doc
            .texture_sets
            .iter()
            .find(|set| set.id == inactive_id)
            .unwrap();
        assert_eq!(inactive.name, "Renamed inactive");
        assert_eq!(
            inactive
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            inactive_layer_ids
        );
        assert_eq!(selected_root_index(&inactive.layer_tree), inactive_selected);
        assert_eq!(inactive.layer_tree.next_node_id, inactive_next_id);
        assert_eq!(doc.active_set_id, active_id);
        assert_eq!(doc.active_set_name, active_name);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            active_layers
        );
    }

    #[test]
    fn texture_set_rename_rejects_invalid_ids_and_names_without_mutation() {
        let mut doc = Document::new(64);
        let inactive_id = doc.add_texture_set("Inactive").unwrap();
        doc.activate_texture_set(1).unwrap();
        let active_name = doc.active_set_name.clone();
        let inactive_name = doc.texture_sets[0].name.clone();
        let active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let active_selected = selected_root_index(&doc.layer_tree);
        let active_next_id = doc.layer_tree.next_node_id;
        let bad_events = [
            DocumentEvent::TextureSetRename {
                set_id: 0,
                name: "Invalid zero ID".into(),
            },
            DocumentEvent::TextureSetRename {
                set_id: inactive_id + 100,
                name: "Unknown set".into(),
            },
            DocumentEvent::TextureSetRename {
                set_id: doc.active_set_id,
                name: String::new(),
            },
            DocumentEvent::TextureSetRename {
                set_id: doc.active_set_id,
                name: " \t\n ".into(),
            },
            DocumentEvent::TextureSetRename {
                set_id: doc.active_set_id,
                name: "x".repeat(129),
            },
            DocumentEvent::TextureSetRename {
                set_id: doc.active_set_id,
                name: "é".repeat(65),
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(doc.active_set_name, active_name);
            assert_eq!(doc.texture_sets[0].name, inactive_name);
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                active_ids
            );
            assert_eq!(selected_root_index(&doc.layer_tree), active_selected);
            assert_eq!(doc.layer_tree.next_node_id, active_next_id);
        }
    }

    #[test]
    fn texture_set_create_codec_activates_new_set_and_preserves_prior_state() {
        let mut doc = Document::new(64);
        let existing_id = doc.add_texture_set("Existing").unwrap();
        let existing_layer_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 9, image::Rgba([11, 22, 33, 255]));
        doc.selected_layer_mut().unwrap().extensions.insert(
            "future_layer".into(),
            serde_json::json!({ "preserve": true }),
        );
        doc.material_bindings = vec![Some(1), Some(existing_id), None];
        doc.activate_texture_set(1).unwrap();
        let prior_active_layer_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(4, 5, image::Luma([17]));
        doc.active_set_extensions
            .insert("future_set".into(), serde_json::json!("preserve"));
        let previous_bindings = doc.material_bindings.clone();
        let created_id = doc.next_set_id;
        let event = DocumentEvent::TextureSetCreate {
            set_id: created_id,
            name: "  New Texture Set  ".into(),
        };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        assert_eq!(doc.active_set_id, created_id);
        assert_eq!(doc.active_set_name, "New Texture Set");
        assert_eq!(doc.size, 64);
        assert_eq!(selected_root_index(&doc.layer_tree), 0);
        assert_eq!(doc.layer_tree.next_node_id, 2);
        assert_eq!(doc.next_set_id, created_id + 1);
        assert_eq!(doc.layer_tree.roots.len(), 1);
        assert_eq!(doc.layer_tree.roots[0].id, 1);
        assert_eq!(doc.layer_tree.roots[0].name, "Paint Layer");
        assert!(doc.layer_tree.roots[0].pixels.is_empty());
        assert!(doc.layer_tree.roots[0]
            .mask
            .pixels()
            .all(|pixel| pixel.0 == [255]));
        assert_eq!(doc.material_bindings, previous_bindings);

        let previous_active = doc.texture_sets.iter().find(|set| set.id == 1).unwrap();
        assert_eq!(
            previous_active.layer_tree.roots[0].id,
            prior_active_layer_id
        );
        assert_eq!(
            previous_active.layer_tree.roots[0].mask.get_pixel(4, 5).0,
            [17]
        );
        assert_eq!(previous_active.extensions["future_set"], "preserve");
        let existing = doc
            .texture_sets
            .iter()
            .find(|set| set.id == existing_id)
            .unwrap();
        assert_eq!(existing.layer_tree.roots[0].id, existing_layer_id);
        assert_eq!(
            existing.layer_tree.roots[0].pixels[&Channel::BaseColor]
                .get_pixel(7, 9)
                .0,
            [11, 22, 33, 255]
        );
        assert_eq!(
            existing.layer_tree.roots[0].extensions["future_layer"],
            serde_json::json!({ "preserve": true })
        );
    }

    #[test]
    fn texture_set_create_rejects_invalid_inputs_without_mutation() {
        let mut doc = Document::new(64);
        let next_id = doc.next_set_id;
        let before_active_id = doc.active_set_id;
        let before_active_name = doc.active_set_name.clone();
        let before_active_layers = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_next_layer_id = doc.layer_tree.next_node_id;
        let before_next_set_id = doc.next_set_id;
        let before_decoded_bytes = doc.decoded_bytes();
        let bad_events = [
            DocumentEvent::TextureSetCreate {
                set_id: 0,
                name: "Zero ID".into(),
            },
            DocumentEvent::TextureSetCreate {
                set_id: next_id + 1,
                name: "Skipped ID".into(),
            },
            DocumentEvent::TextureSetCreate {
                set_id: next_id,
                name: String::new(),
            },
            DocumentEvent::TextureSetCreate {
                set_id: next_id,
                name: " \t\n ".into(),
            },
            DocumentEvent::TextureSetCreate {
                set_id: next_id,
                name: "x".repeat(129),
            },
            DocumentEvent::TextureSetCreate {
                set_id: next_id,
                name: "é".repeat(65),
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(doc.active_set_id, before_active_id);
            assert_eq!(doc.active_set_name, before_active_name);
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before_active_layers
            );
            assert_eq!(doc.layer_tree.next_node_id, before_next_layer_id);
            assert_eq!(doc.next_set_id, before_next_set_id);
            assert_eq!(doc.texture_sets.len(), 0);
            assert_eq!(doc.decoded_bytes(), before_decoded_bytes);
        }

        let mut max_sets = Document::new(64);
        while max_sets.texture_set_count() < MAX_TEXTURE_SETS {
            let name = format!("Set {}", max_sets.next_set_id);
            max_sets.add_texture_set(&name).unwrap();
        }
        let max_next_id = max_sets.next_set_id;
        let max_count = max_sets.texture_set_count();
        assert!(DocumentEvent::TextureSetCreate {
            set_id: max_next_id,
            name: "Over limit".into(),
        }
        .apply(&mut max_sets)
        .is_err());
        assert_eq!(max_sets.texture_set_count(), max_count);
        assert_eq!(max_sets.next_set_id, max_next_id);

        let mut exhausted = Document::new(64);
        exhausted.next_set_id = u32::MAX;
        assert!(DocumentEvent::TextureSetCreate {
            set_id: u32::MAX,
            name: "Counter overflow".into(),
        }
        .apply(&mut exhausted)
        .is_err());
        assert!(exhausted.texture_sets.is_empty());
        assert_eq!(exhausted.active_set_id, 1);
        assert_eq!(exhausted.next_set_id, u32::MAX);

        let mut invalid_resolution = Document::new(64);
        invalid_resolution.size = MAX_SIZE + 1;
        assert!(DocumentEvent::TextureSetCreate {
            set_id: invalid_resolution.next_set_id,
            name: "Invalid resolution".into(),
        }
        .apply(&mut invalid_resolution)
        .is_err());
        assert!(invalid_resolution.texture_sets.is_empty());
        assert_eq!(invalid_resolution.active_set_id, 1);
    }

    #[test]
    fn texture_set_create_decoded_budget_counts_only_the_blank_mask() {
        let size = MAX_SIZE;
        let mask_bytes = u64::from(size) * u64::from(size);
        assert!(texture_set_allocation_fits(
            MAX_DECODED_BYTES - mask_bytes,
            size
        ));
        assert!(!texture_set_allocation_fits(
            MAX_DECODED_BYTES - mask_bytes + 1,
            size
        ));
        assert!(!texture_set_allocation_fits(u64::MAX, size));
    }

    #[test]
    fn texture_set_remove_codec_selects_lowest_survivor_and_clears_bindings() {
        let mut doc = Document::new(64);
        let set_two = doc.add_texture_set("Two").unwrap();
        let set_three = doc.add_texture_set("Three").unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(8, 9, image::Rgba([42, 43, 44, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .extensions
            .insert("to_remove".into(), serde_json::json!(true));
        doc.material_bindings = vec![Some(set_three), Some(set_two), None];
        doc.texture_sets.reverse();
        let removed_id = doc.active_set_id;
        let next_set_id = doc.next_set_id;
        let event = DocumentEvent::TextureSetRemove { set_id: removed_id };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        assert_eq!(doc.active_set_id, 1);
        assert_eq!(doc.active_set_name, "Default");
        assert_eq!(doc.next_set_id, next_set_id);
        assert_eq!(doc.texture_set_count(), 2);
        assert_eq!(doc.material_bindings, vec![None, Some(set_two), None]);
        assert!(!doc.texture_sets.iter().any(|set| set.id == removed_id));
        assert!(doc.layer_tree.roots[0].pixels.is_empty());
        let survivor = doc
            .texture_sets
            .iter()
            .find(|set| set.id == set_two)
            .unwrap();
        assert_eq!(survivor.name, "Two");
    }

    #[test]
    fn texture_set_remove_rejects_invalid_inputs_without_mutation() {
        let mut doc = Document::new(64);
        let set_id = doc.add_texture_set("Remove Target").unwrap();
        let target_name = doc.active_set_name.clone();
        let target_next_layer_id = doc.layer_tree.next_node_id;
        let next_set_id = doc.next_set_id;
        let active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let inactive_ids = doc
            .texture_sets
            .iter()
            .map(|set| set.id)
            .collect::<Vec<_>>();
        let before_bindings = doc.material_bindings.clone();
        let bad_events = [
            DocumentEvent::TextureSetRemove { set_id: 0 },
            DocumentEvent::TextureSetRemove { set_id: 1 },
            DocumentEvent::TextureSetRemove {
                set_id: set_id + 100,
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(doc.active_set_id, set_id);
            assert_eq!(doc.active_set_name, target_name);
            assert_eq!(doc.layer_tree.next_node_id, target_next_layer_id);
            assert_eq!(doc.next_set_id, next_set_id);
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                active_ids
            );
            assert_eq!(
                doc.texture_sets
                    .iter()
                    .map(|set| set.id)
                    .collect::<Vec<_>>(),
                inactive_ids
            );
            assert_eq!(doc.material_bindings, before_bindings);
        }

        let mut only_set = Document::new(64);
        let only_id = only_set.active_set_id;
        let only_layer_id = only_set.selected_layer().unwrap().id;
        assert!(DocumentEvent::TextureSetRemove { set_id: only_id }
            .apply(&mut only_set)
            .is_err());
        assert_eq!(only_set.active_set_id, only_id);
        assert_eq!(only_set.layer_tree.roots[0].id, only_layer_id);
        assert_eq!(only_set.texture_sets.len(), 0);
    }

    #[test]
    fn texture_set_select_codec_and_active_inactive_replay_preserve_set_state() {
        let mut doc = Document::new(64);
        let first_set_id = doc.active_set_id;
        let second_set_id = doc.add_texture_set("Second").unwrap();
        doc.add_layer().unwrap();
        doc.layer_tree.select(doc.layer_tree.roots[0].id()).unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(3, 4, image::Rgba([9, 8, 7, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(5, 6, image::Luma([37]));
        doc.selected_layer_mut().unwrap().name = "Base in second".into();
        doc.layer_tree.select(doc.layer_tree.roots[1].id()).unwrap();
        doc.selected_layer_mut().unwrap().name = "Detail in second".into();
        let second_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let second_selected = selected_root_index(&doc.layer_tree);
        let second_next_layer_id = doc.layer_tree.next_node_id;
        let second_size = doc.size;

        let third_set_id = doc.add_texture_set("Third").unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(8, 9, image::Luma([21]));
        doc.material_bindings = vec![Some(second_set_id)];
        let binding_before = doc.material_bindings.clone();
        let event = DocumentEvent::TextureSetSelect {
            set_id: second_set_id,
        };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        assert_eq!(doc.active_set_id, second_set_id);
        assert_eq!(doc.active_set_name, "Second");
        assert_eq!(doc.size, second_size);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            second_ids
        );
        assert_eq!(selected_root_index(&doc.layer_tree), second_selected);
        assert_eq!(doc.layer_tree.next_node_id, second_next_layer_id);
        assert_eq!(doc.layer_tree.roots[0].name, "Base in second");
        assert_eq!(doc.layer_tree.roots[0].mask.get_pixel(5, 6).0, [37]);
        assert_eq!(
            doc.layer_tree.roots[0].pixels[&Channel::BaseColor]
                .get_pixel(3, 4)
                .0,
            [9, 8, 7, 255]
        );
        assert_eq!(doc.material_bindings, binding_before);
        let third = doc
            .texture_sets
            .iter()
            .find(|set| set.id == third_set_id)
            .unwrap();
        assert_eq!(third.size, 64);
        assert_eq!(selected_root_index(&third.layer_tree), 0);
        assert_eq!(third.layer_tree.next_node_id, 2);
        assert_eq!(third.layer_tree.roots[0].mask.get_pixel(8, 9).0, [21]);

        // Selecting the active ID is a validated no-op.
        let before_active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        DocumentEvent::TextureSetSelect {
            set_id: second_set_id,
        }
        .apply(&mut doc)
        .unwrap();
        assert_eq!(doc.active_set_id, second_set_id);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_active_ids
        );
        assert_eq!(doc.material_bindings, binding_before);
        assert!(doc.texture_sets.iter().any(|set| set.id == first_set_id));
    }

    #[test]
    fn texture_set_select_rejects_invalid_ids_without_mutation() {
        let mut doc = Document::new(64);
        let second_set_id = doc.add_texture_set("Second").unwrap();
        let active_id = doc.active_set_id;
        let active_name = doc.active_set_name.clone();
        let active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let selected = selected_root_index(&doc.layer_tree);
        let next_layer_id = doc.layer_tree.next_node_id;
        let next_set_id = doc.next_set_id;
        let inactive_ids = doc
            .texture_sets
            .iter()
            .map(|set| set.id)
            .collect::<Vec<_>>();
        let before_bindings = doc.material_bindings.clone();
        for target_id in [0, second_set_id + 100] {
            assert!(DocumentEvent::TextureSetSelect { set_id: target_id }
                .apply(&mut doc)
                .is_err());
            assert_eq!(doc.active_set_id, active_id);
            assert_eq!(doc.active_set_name, active_name);
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                active_ids
            );
            assert_eq!(selected_root_index(&doc.layer_tree), selected);
            assert_eq!(doc.layer_tree.next_node_id, next_layer_id);
            assert_eq!(doc.next_set_id, next_set_id);
            assert_eq!(
                doc.texture_sets
                    .iter()
                    .map(|set| set.id)
                    .collect::<Vec<_>>(),
                inactive_ids
            );
            assert_eq!(doc.material_bindings, before_bindings);
        }

        // Even an active-set no-op fails closed if another set aliases its ID.
        doc.texture_sets[0].id = active_id;
        let before_ids = doc
            .texture_sets
            .iter()
            .map(|set| set.id)
            .collect::<Vec<_>>();
        assert!(DocumentEvent::TextureSetSelect { set_id: active_id }
            .apply(&mut doc)
            .is_err());
        assert_eq!(doc.active_set_id, active_id);
        assert_eq!(
            doc.texture_sets
                .iter()
                .map(|set| set.id)
                .collect::<Vec<_>>(),
            before_ids
        );

        let mut invalid_counter = Document::new(64);
        invalid_counter.next_set_id = invalid_counter.active_set_id;
        assert!(DocumentEvent::TextureSetSelect {
            set_id: invalid_counter.active_set_id,
        }
        .apply(&mut invalid_counter)
        .is_err());
        assert_eq!(invalid_counter.active_set_id, 1);
        assert_eq!(invalid_counter.texture_sets.len(), 0);
    }

    #[test]
    fn texture_set_resize_replays_active_set_and_rejects_invalid_targets() {
        let mut doc = Document::new(64);
        let active_id = doc.active_set_id;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(2, 3, image::Rgba([17, 34, 51, 68]));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Roughness, 64)
            .put_pixel(4, 5, image::Rgba([91, 0, 0, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(6, 7, image::Luma([123]));
        let inactive_id = doc.add_texture_set("Inactive").unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Metallic, 64)
            .put_pixel(8, 9, image::Rgba([177, 0, 0, 255]));
        let inactive_id_before = doc.active_set_id;
        let inactive_size_before = doc.size;
        let inactive_layers_before = doc.layer_tree.roots.clone();
        doc.activate_texture_set(active_id).unwrap();
        let before = doc.layer_tree.roots.clone();

        let event = DocumentEvent::TextureSetResize {
            set_id: active_id,
            size: 128,
        };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        decoded.apply(&mut doc).unwrap();
        assert_eq!(doc.size, 128);
        assert_eq!(doc.active_set_id, active_id);
        for (actual, old) in doc.layer_tree.roots.iter().zip(before.iter()) {
            assert_eq!(
                actual.mask,
                image::imageops::resize(&old.mask, 128, 128, image::imageops::FilterType::Nearest,)
            );
            assert_eq!(actual.pixels.len(), old.pixels.len());
            for (channel, pixels) in &old.pixels {
                assert_eq!(
                    actual.pixels[channel],
                    image::imageops::resize(pixels, 128, 128, image::imageops::FilterType::Nearest,)
                );
            }
        }
        let inactive_set = doc
            .texture_sets
            .iter()
            .find(|set| set.id == inactive_id)
            .unwrap();
        assert_eq!(inactive_set.id, inactive_id_before);
        assert_eq!(inactive_set.size, inactive_size_before);
        for (actual, before) in inactive_set
            .layer_tree
            .roots
            .iter()
            .zip(&inactive_layers_before)
        {
            assert_eq!(actual.mask, before.mask);
            assert_eq!(actual.pixels, before.pixels);
        }

        for invalid in [
            DocumentEvent::TextureSetResize {
                set_id: inactive_id,
                size: 128,
            },
            DocumentEvent::TextureSetResize {
                set_id: active_id,
                size: 63,
            },
            DocumentEvent::TextureSetResize {
                set_id: 0,
                size: 128,
            },
        ] {
            let size_before = doc.size;
            let active_id_before = doc.active_set_id;
            let layers_before = doc.layer_tree.roots.clone();
            assert!(invalid.apply(&mut doc).is_err());
            assert_eq!(doc.size, size_before);
            assert_eq!(doc.active_set_id, active_id_before);
            assert_eq!(doc.layer_tree.roots.len(), layers_before.len());
            for (actual, before) in doc.layer_tree.roots.iter().zip(&layers_before) {
                assert_eq!(actual.mask, before.mask);
                assert_eq!(actual.pixels, before.pixels);
            }
        }
    }

    #[test]
    fn texture_set_ao_settings_are_bounded_set_scoped_and_replayable() {
        let mut doc = Document::new(64);
        let first_id = doc.active_set_id;
        doc.active_set_extensions.insert(
            crate::bake::AO_SETTINGS_EXTENSION_KEY.into(),
            serde_json::json!({"futureOption": {"retain": true}}),
        );
        let event = DocumentEvent::TextureSetAoSettings {
            set_id: first_id,
            sample_count: 23,
            max_ray_distance: 4.25,
            seed: 912,
        };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        decoded.apply(&mut doc).unwrap();
        let first_settings: crate::bake::AmbientOcclusionBakeSettings = serde_json::from_value(
            doc.active_set_extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY].clone(),
        )
        .unwrap();
        assert_eq!(
            first_settings,
            crate::bake::AmbientOcclusionBakeSettings {
                sample_count: 23,
                max_ray_distance: 4.25,
                seed: 912,
            }
        );
        assert_eq!(
            doc.active_set_extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY]["futureOption"],
            serde_json::json!({"retain": true})
        );

        let second_id = doc.add_texture_set("Second").unwrap();
        let second_event = DocumentEvent::TextureSetAoSettings {
            set_id: second_id,
            sample_count: 8,
            max_ray_distance: 2.0,
            seed: 31,
        };
        second_event.apply(&mut doc).unwrap();
        let second = doc
            .texture_sets
            .iter()
            .find(|set| set.id == first_id)
            .unwrap();
        assert_eq!(
            serde_json::from_value::<crate::bake::AmbientOcclusionBakeSettings>(
                second.extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY].clone()
            )
            .unwrap(),
            first_settings
        );

        for invalid in [
            DocumentEvent::TextureSetAoSettings {
                set_id: second_id,
                sample_count: 0,
                max_ray_distance: 2.0,
                seed: 0,
            },
            DocumentEvent::TextureSetAoSettings {
                set_id: second_id,
                sample_count: 1,
                max_ray_distance: f32::NAN,
                seed: 0,
            },
            DocumentEvent::TextureSetAoSettings {
                set_id: 999,
                sample_count: 1,
                max_ray_distance: 2.0,
                seed: 0,
            },
        ] {
            let before = doc.active_set_extensions.clone();
            assert!(invalid.apply(&mut doc).is_err());
            assert_eq!(doc.active_set_extensions, before);
        }
    }

    #[test]
    fn material_binding_codec_binds_and_unbinds_slots() {
        let mut doc = Document::new(64);
        let inactive_set_id = doc.add_texture_set("Bound Set").unwrap();
        doc.activate_texture_set(1).unwrap();
        doc.mesh.materials.push("Second slot".into());
        let material_count = doc.mesh.material_count();
        assert!(material_count >= 2);
        doc.material_bindings = (0..material_count)
            .map(|slot| Some(if slot == 0 { 1 } else { inactive_set_id }))
            .collect();

        let active_bind = DocumentEvent::MaterialBinding {
            slot: 0,
            set_id: Some(1),
        };
        let decoded_active_bind =
            DocumentEvent::decode_bounded(&active_bind.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded_active_bind, active_bind);
        decoded_active_bind.apply(&mut doc).unwrap();
        assert_eq!(doc.material_bindings[0], Some(1));

        let bind = DocumentEvent::MaterialBinding {
            slot: 0,
            set_id: Some(inactive_set_id),
        };
        let decoded_bind = DocumentEvent::decode_bounded(&bind.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded_bind, bind);
        decoded_bind.apply(&mut doc).unwrap();
        assert_eq!(doc.material_bindings[0], Some(inactive_set_id));
        assert_eq!(doc.material_bindings[1], Some(inactive_set_id));
        assert_eq!(doc.material_bindings.len(), material_count);

        let unbind = DocumentEvent::MaterialBinding {
            slot: 1,
            set_id: None,
        };
        let decoded_unbind =
            DocumentEvent::decode_bounded(&unbind.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded_unbind, unbind);
        decoded_unbind.apply(&mut doc).unwrap();
        assert_eq!(doc.material_bindings[0], Some(inactive_set_id));
        assert_eq!(doc.material_bindings[1], None);
        assert_eq!(doc.material_bindings.len(), material_count);
    }

    #[test]
    fn material_binding_replay_safely_extends_short_vector_and_preserves_long_vector() {
        let mut doc = Document::new(64);
        let set_id = doc.add_texture_set("Bound Set").unwrap();
        doc.activate_texture_set(1).unwrap();
        doc.mesh.materials.push("Second slot".into());
        let material_count = doc.mesh.material_count();

        doc.material_bindings.clear();
        DocumentEvent::MaterialBinding {
            slot: material_count - 1,
            set_id: Some(set_id),
        }
        .apply(&mut doc)
        .unwrap();
        assert_eq!(doc.material_bindings.len(), material_count);
        assert!(doc.material_bindings[..material_count - 1]
            .iter()
            .all(Option::is_none));
        assert_eq!(doc.material_bindings[material_count - 1], Some(set_id));

        doc.material_bindings.push(Some(1));
        let overlong_len = doc.material_bindings.len();
        DocumentEvent::MaterialBinding {
            slot: 0,
            set_id: None,
        }
        .apply(&mut doc)
        .unwrap();
        assert_eq!(doc.material_bindings.len(), overlong_len);
        assert_eq!(doc.material_bindings[0], None);
        assert_eq!(doc.material_bindings[material_count - 1], Some(set_id));
        assert_eq!(doc.material_bindings[material_count], Some(1));
    }

    #[test]
    fn material_binding_rejects_unknown_ids_and_out_of_range_slots_without_mutation() {
        let mut doc = Document::new(64);
        let known_id = doc.add_texture_set("Known").unwrap();
        doc.activate_texture_set(1).unwrap();
        doc.mesh.materials.push("Second slot".into());
        doc.material_bindings = vec![Some(1), Some(known_id)];
        let before = doc.material_bindings.clone();
        let bad_events = [
            DocumentEvent::MaterialBinding {
                slot: doc.mesh.material_count(),
                set_id: None,
            },
            DocumentEvent::MaterialBinding {
                slot: 0,
                set_id: Some(0),
            },
            DocumentEvent::MaterialBinding {
                slot: 0,
                set_id: Some(known_id + 100),
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(doc.material_bindings, before);
        }

        // Stable IDs must resolve to exactly one set, even in a malformed
        // candidate document with duplicate IDs.
        doc.texture_sets[0].id = doc.active_set_id;
        let before = doc.material_bindings.clone();
        assert!(DocumentEvent::MaterialBinding {
            slot: 0,
            set_id: Some(doc.active_set_id),
        }
        .apply(&mut doc)
        .is_err());
        assert_eq!(doc.material_bindings, before);
    }

    #[test]
    fn layer_duplicate_codec_and_replay_preserve_full_active_layer_state() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let source_id = doc.selected_layer().unwrap().id;
        let size = doc.size;
        {
            let source = doc.selected_layer_mut().unwrap();
            source.pixels_mut(Channel::BaseColor, size).put_pixel(
                2,
                3,
                image::Rgba([12, 34, 56, 255]),
            );
            source.pixels_mut(Channel::Roughness, size).put_pixel(
                5,
                6,
                image::Rgba([77, 0, 0, 255]),
            );
            source.mask.put_pixel(4, 5, image::Luma([73]));
            source.name = "Authored detail".into();
            source.visible = false;
            source.opacity = 0.375;
            source.blend = BlendMode::Detail;
            source.disabled_channels.insert(Channel::Metallic);
            source
                .extensions
                .insert("future".into(), serde_json::json!({ "keep": [1, 2, 3] }));
        }
        let source_before = doc.selected_layer().unwrap().clone();
        let duplicate_id = doc.layer_tree.next_node_id;
        let event = DocumentEvent::LayerDuplicate {
            set_id: doc.active_set_id,
            source_layer_id: source_id,
            layer_id: duplicate_id,
            to_index: selected_root_index(&doc.layer_tree) + 1,
        };
        let decoded = DocumentEvent::decode_bounded(&event.encode_bounded().unwrap()).unwrap();
        assert_eq!(decoded, event);
        decoded.apply(&mut doc).unwrap();

        let duplicate = &doc.layer_tree.roots[selected_root_index(&doc.layer_tree)];
        assert_eq!(duplicate.id, duplicate_id);
        assert_eq!(duplicate.name, "Authored detail copy");
        assert_eq!(duplicate.visible, source_before.visible);
        assert_eq!(duplicate.opacity, source_before.opacity);
        assert_eq!(duplicate.blend, source_before.blend);
        assert_eq!(duplicate.pixels, source_before.pixels);
        assert_eq!(duplicate.mask, source_before.mask);
        assert_eq!(duplicate.disabled_channels, source_before.disabled_channels);
        assert_eq!(duplicate.extensions, source_before.extensions);
        assert_eq!(doc.selected_layer().unwrap().id, duplicate_id);
        assert_eq!(selected_root_index(&doc.layer_tree), 2);
        assert_eq!(doc.layer_tree.next_node_id, duplicate_id + 1);
    }

    #[test]
    fn layer_duplicate_replay_supports_inactive_texture_sets_only() {
        let mut doc = Document::new(64);
        let inactive_set_id = doc.add_texture_set("Recovery Set").unwrap();
        doc.add_layer().unwrap();
        let source_id = doc.selected_layer().unwrap().id;
        let duplicate_id = doc.layer_tree.next_node_id;
        let expected_source = doc.selected_layer().unwrap().clone();
        doc.activate_texture_set(1).unwrap();
        let active_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let active_next_id = doc.layer_tree.next_node_id;

        DocumentEvent::LayerDuplicate {
            set_id: inactive_set_id,
            source_layer_id: source_id,
            layer_id: duplicate_id,
            to_index: 2,
        }
        .apply(&mut doc)
        .unwrap();

        let inactive = doc
            .texture_sets
            .iter()
            .find(|set| set.id == inactive_set_id)
            .unwrap();
        assert_eq!(inactive.layer_tree.roots[2].id, duplicate_id);
        assert_eq!(
            inactive.layer_tree.roots[2].name,
            format!("{} copy", expected_source.name)
        );
        assert_eq!(inactive.layer_tree.roots[2].pixels, expected_source.pixels);
        assert_eq!(selected_root_index(&inactive.layer_tree), 2);
        assert_eq!(inactive.layer_tree.next_node_id, duplicate_id + 1);
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            active_ids
        );
        assert_eq!(doc.layer_tree.next_node_id, active_next_id);
    }

    #[test]
    fn layer_duplicate_rejects_invalid_inputs_without_mutation() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let source_id = doc.selected_layer().unwrap().id;
        let target_id = doc.layer_tree.next_node_id;
        let selected = selected_root_index(&doc.layer_tree);
        let next_layer_id = doc.layer_tree.next_node_id;
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_decoded_bytes = doc.decoded_bytes();
        let bad_events = [
            DocumentEvent::LayerDuplicate {
                set_id: 0,
                source_layer_id: source_id,
                layer_id: target_id,
                to_index: selected + 1,
            },
            DocumentEvent::LayerDuplicate {
                set_id: doc.active_set_id + 100,
                source_layer_id: source_id,
                layer_id: target_id,
                to_index: selected + 1,
            },
            DocumentEvent::LayerDuplicate {
                set_id: doc.active_set_id,
                source_layer_id: 0,
                layer_id: target_id,
                to_index: selected + 1,
            },
            DocumentEvent::LayerDuplicate {
                set_id: doc.active_set_id,
                source_layer_id: source_id,
                layer_id: 0,
                to_index: selected + 1,
            },
            DocumentEvent::LayerDuplicate {
                set_id: doc.active_set_id,
                source_layer_id: source_id - 1,
                layer_id: target_id,
                to_index: selected + 1,
            },
            DocumentEvent::LayerDuplicate {
                set_id: doc.active_set_id,
                source_layer_id: source_id,
                layer_id: target_id + 1,
                to_index: selected + 1,
            },
            DocumentEvent::LayerDuplicate {
                set_id: doc.active_set_id,
                source_layer_id: source_id,
                layer_id: target_id,
                to_index: selected,
            },
        ];
        for event in bad_events {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before_ids
            );
            assert_eq!(selected_root_index(&doc.layer_tree), selected);
            assert_eq!(doc.layer_tree.next_node_id, next_layer_id);
            assert_eq!(doc.decoded_bytes(), before_decoded_bytes);
        }

        doc.selected_layer_mut().unwrap().name = "n".repeat(1020);
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_next_id = doc.layer_tree.next_node_id;
        let before_selected = selected_root_index(&doc.layer_tree);
        let before_decoded_bytes = doc.decoded_bytes();
        assert!(DocumentEvent::LayerDuplicate {
            set_id: doc.active_set_id,
            source_layer_id: source_id,
            layer_id: target_id,
            to_index: selected + 1,
        }
        .apply(&mut doc)
        .is_err());
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(selected_root_index(&doc.layer_tree), before_selected);
        assert_eq!(doc.layer_tree.next_node_id, before_next_id);
        assert_eq!(doc.decoded_bytes(), before_decoded_bytes);
    }

    #[test]
    fn layer_duplicate_rejects_layer_limit_and_counter_overflow() {
        let mut full = Document::new(64);
        while full.layer_tree.roots.len() < MAX_LAYERS {
            full.add_layer().unwrap();
        }
        let full_selected_id = full.selected_layer().unwrap().id;
        let full_next_id = full.layer_tree.next_node_id;
        let full_selected = selected_root_index(&full.layer_tree);
        assert!(DocumentEvent::LayerDuplicate {
            set_id: full.active_set_id,
            source_layer_id: full_selected_id,
            layer_id: full_next_id,
            to_index: full_selected + 1,
        }
        .apply(&mut full)
        .is_err());
        assert_eq!(full.layer_tree.roots.len(), MAX_LAYERS);
        assert_eq!(selected_root_index(&full.layer_tree), full_selected);
        assert_eq!(full.layer_tree.next_node_id, full_next_id);

        let mut exhausted = Document::new(64);
        exhausted.add_layer().unwrap();
        exhausted.layer_tree.next_node_id = u32::MAX;
        let source_id = exhausted.selected_layer().unwrap().id;
        let before_ids = exhausted
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_selected = selected_root_index(&exhausted.layer_tree);
        assert!(DocumentEvent::LayerDuplicate {
            set_id: exhausted.active_set_id,
            source_layer_id: source_id,
            layer_id: u32::MAX,
            to_index: selected_root_index(&exhausted.layer_tree) + 1,
        }
        .apply(&mut exhausted)
        .is_err());
        assert_eq!(
            exhausted
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(selected_root_index(&exhausted.layer_tree), before_selected);
        assert_eq!(exhausted.layer_tree.next_node_id, u32::MAX);
    }

    #[test]
    fn layer_duplicate_rejects_aggregate_extension_clone_cap_without_mutation() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let large_extension = "x".repeat(24 * 1024 * 1024);
        doc.selected_layer_mut()
            .unwrap()
            .extensions
            .insert("opaque".into(), serde_json::Value::String(large_extension));
        let source_id = doc.selected_layer().unwrap().id;
        let target_id = doc.layer_tree.next_node_id;
        let selected = selected_root_index(&doc.layer_tree);
        let before_ids = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_next_id = doc.layer_tree.next_node_id;
        assert!(DocumentEvent::LayerDuplicate {
            set_id: doc.active_set_id,
            source_layer_id: source_id,
            layer_id: target_id,
            to_index: selected + 1,
        }
        .apply(&mut doc)
        .is_err());
        assert_eq!(
            doc.layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(selected_root_index(&doc.layer_tree), selected);
        assert_eq!(doc.layer_tree.next_node_id, before_next_id);
        assert_eq!(
            doc.selected_layer().unwrap().extensions["opaque"]
                .as_str()
                .unwrap()
                .len(),
            24 * 1024 * 1024
        );
    }

    #[test]
    fn layer_duplicate_storage_projection_includes_mask_and_each_channel() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, size);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, size);
        let source = doc.selected_layer().unwrap();
        let expected = u64::from(size) * u64::from(size) * 9;
        assert_eq!(layer_storage_bytes_checked(source, size).unwrap(), expected);
        assert!(layer_allocation_fits(
            MAX_DECODED_BYTES - expected,
            expected
        ));
        assert!(!layer_allocation_fits(
            MAX_DECODED_BYTES - expected + 1,
            expected
        ));

        let mut malformed = Document::new(64);
        malformed.add_layer().unwrap();
        malformed
            .selected_layer_mut()
            .unwrap()
            .pixels
            .insert(Channel::BaseColor, RgbaImage::new(1, 1));
        assert!(layer_storage_bytes_checked(malformed.selected_layer().unwrap(), 64).is_err());

        let source_id = malformed.selected_layer().unwrap().id;
        let target_id = malformed.layer_tree.next_node_id;
        let before_ids = malformed
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();
        let before_selected = selected_root_index(&malformed.layer_tree);
        let before_next_id = malformed.layer_tree.next_node_id;
        assert!(DocumentEvent::LayerDuplicate {
            set_id: malformed.active_set_id,
            source_layer_id: source_id,
            layer_id: target_id,
            to_index: selected_root_index(&malformed.layer_tree) + 1,
        }
        .apply(&mut malformed)
        .is_err());
        assert_eq!(
            malformed
                .layer_tree
                .roots
                .iter()
                .map(|layer| layer.id)
                .collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(selected_root_index(&malformed.layer_tree), before_selected);
        assert_eq!(malformed.layer_tree.next_node_id, before_next_id);
    }

    #[test]
    fn structural_events_reject_invalid_ids_indices_and_values_without_mutation() {
        let mut doc = Document::new(64);
        let layer_id = doc.layer_tree.roots[0].id;
        doc.add_layer().unwrap();
        let before = doc
            .layer_tree
            .roots
            .iter()
            .map(|layer| layer.id)
            .collect::<Vec<_>>();

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
                doc.layer_tree
                    .roots
                    .iter()
                    .map(|layer| layer.id)
                    .collect::<Vec<_>>(),
                before
            );
        }
        assert_eq!(doc.layer_tree.roots[0].name, "Paint Layer");
        assert_eq!(doc.layer_tree.roots[0].opacity, 1.0);
    }

    #[test]
    fn layer_blend_mode_roundtrips_and_replays_every_mode() {
        let mut doc = Document::new(64);
        let layer_id = doc.layer_tree.roots[0].id;

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
            assert_eq!(doc.layer_tree.roots[0].blend, blend);
        }
    }

    #[test]
    fn procedural_graph_event_roundtrips_sets_clears_and_rejects_invalid_graphs() {
        let mut doc = Document::new(64);
        let layer_id = doc.layer_tree.roots[0].id;
        let graph = crate::procedural::ProceduralGraph::value_noise_times_occlusion(47, 3.25, 81);
        let set = DocumentEvent::LayerProceduralGraphSet {
            set_id: doc.active_set_id,
            layer_id,
            graph: Some(graph.clone()),
        };
        let encoded = set.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        assert_eq!(decoded, set);
        decoded.apply(&mut doc).unwrap();
        assert_eq!(doc.layer_tree.roots[0].procedural_graph, Some(graph));

        DocumentEvent::LayerProceduralGraphSet {
            set_id: doc.active_set_id,
            layer_id,
            graph: None,
        }
        .apply(&mut doc)
        .unwrap();
        assert!(doc.layer_tree.roots[0].procedural_graph.is_none());

        let invalid = DocumentEvent::LayerProceduralGraphSet {
            set_id: doc.active_set_id,
            layer_id,
            graph: Some(
                crate::procedural::ProceduralGraph::value_noise_times_occlusion(47, f32::NAN, 81),
            ),
        };
        assert!(invalid.apply(&mut doc).is_err());
        assert!(doc.layer_tree.roots[0].procedural_graph.is_none());
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
            .layer_tree
            .roots[0]
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
                .layer_tree
                .roots[0]
                .pixels[&Channel::Roughness]
                .get_pixel(1, 2)
                .0,
            [120, 0, 0, 255]
        );
        assert!(!doc.layer_tree.roots[0]
            .pixels
            .contains_key(&Channel::Roughness));
    }

    #[test]
    fn image_fill_event_roundtrips_and_replaces_present_or_absent_channels() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
        let value = [13, 71, 129, 247];
        let absent_event = DocumentEvent::LayerImageFill {
            set_id: doc.active_set_id,
            layer_id,
            channel: Channel::BaseColor,
            value,
        };
        let encoded = absent_event.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        assert_eq!(decoded, absent_event);
        decoded.apply(&mut doc).unwrap();
        let image = &doc.selected_layer().unwrap().pixels[&Channel::BaseColor];
        assert_eq!(image.dimensions(), (64, 64));
        assert!(image.as_raw().chunks_exact(4).all(|pixel| pixel == value));

        let existing = doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64);
        for (index, pixel) in existing.as_mut().chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[index as u8, 4, 8, 255]);
        }
        absent_event.apply(&mut doc).unwrap();
        assert!(doc.selected_layer().unwrap().pixels[&Channel::BaseColor]
            .as_raw()
            .chunks_exact(4)
            .all(|pixel| pixel == value));
    }

    #[test]
    fn mask_set_and_invert_events_replay_exactly_and_invert_roundtrips() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
        for (index, value) in doc
            .selected_layer_mut()
            .unwrap()
            .mask
            .as_mut()
            .iter_mut()
            .enumerate()
        {
            *value = (index % 256) as u8;
        }
        let before = doc.selected_layer().unwrap().mask.clone();
        let invert = DocumentEvent::LayerMaskInvert {
            set_id: doc.active_set_id,
            layer_id,
        };
        let encoded = invert.encode_bounded().unwrap();
        let decoded = DocumentEvent::decode_bounded(&encoded).unwrap();
        assert_eq!(decoded, invert);
        decoded.apply(&mut doc).unwrap();
        assert!(doc
            .selected_layer()
            .unwrap()
            .mask
            .as_raw()
            .iter()
            .zip(before.as_raw())
            .all(|(actual, original)| *actual == !*original));
        invert.apply(&mut doc).unwrap();
        assert_eq!(doc.selected_layer().unwrap().mask, before);

        let fill = DocumentEvent::LayerMaskSet {
            set_id: doc.active_set_id,
            layer_id,
            value: 0xA5,
        };
        let fill = DocumentEvent::decode_bounded(&fill.encode_bounded().unwrap()).unwrap();
        fill.apply(&mut doc).unwrap();
        assert!(doc
            .selected_layer()
            .unwrap()
            .mask
            .as_raw()
            .iter()
            .all(|byte| *byte == 0xA5));
    }

    #[test]
    fn semantic_image_events_reject_invalid_ids_and_dimensions_without_mutation() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
        let original = doc.selected_layer().unwrap().clone();
        for event in [
            DocumentEvent::LayerImageFill {
                set_id: 0,
                layer_id,
                channel: Channel::BaseColor,
                value: [1, 2, 3, 4],
            },
            DocumentEvent::LayerImageFill {
                set_id: doc.active_set_id,
                layer_id: 0,
                channel: Channel::BaseColor,
                value: [1, 2, 3, 4],
            },
            DocumentEvent::LayerMaskInvert {
                set_id: doc.active_set_id,
                layer_id: layer_id + 99,
            },
            DocumentEvent::LayerMaskSet {
                set_id: 0,
                layer_id,
                value: 0,
            },
        ] {
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(
                doc.selected_layer().unwrap().pixels.len(),
                original.pixels.len()
            );
            assert_eq!(doc.selected_layer().unwrap().mask, original.mask);
        }

        doc.selected_layer_mut().unwrap().mask = image::GrayImage::new(1, 1);
        let malformed_mask = doc.selected_layer().unwrap().mask.clone();
        assert!(DocumentEvent::LayerMaskInvert {
            set_id: doc.active_set_id,
            layer_id,
        }
        .apply(&mut doc)
        .is_err());
        assert_eq!(doc.selected_layer().unwrap().mask, malformed_mask);

        doc.selected_layer_mut()
            .unwrap()
            .pixels
            .insert(Channel::BaseColor, RgbaImage::new(1, 1));
        let malformed_channel = doc.selected_layer().unwrap().pixels[&Channel::BaseColor].clone();
        assert!(DocumentEvent::LayerImageFill {
            set_id: doc.active_set_id,
            layer_id,
            channel: Channel::BaseColor,
            value: [5, 6, 7, 8],
        }
        .apply(&mut doc)
        .is_err());
        assert_eq!(
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor],
            malformed_channel
        );
    }

    #[test]
    fn image_patch_replay_rejects_invalid_rectangles_without_mutation() {
        let mut doc = Document::new(64);
        let layer_id = doc.selected_layer().unwrap().id;
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
            .unwrap()
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
        let layer_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut()
            .unwrap()
            .pixels
            .insert(Channel::BaseColor, RgbaImage::new(1, 1));
        let before_channel = doc.selected_layer().unwrap().pixels[&Channel::BaseColor].clone();
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
            doc.selected_layer().unwrap().pixels[&Channel::BaseColor],
            before_channel
        );

        doc.selected_layer_mut().unwrap().mask = image::GrayImage::new(1, 1);
        let before_mask = doc.selected_layer().unwrap().mask.clone();
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
        assert_eq!(doc.selected_layer().unwrap().mask, before_mask);
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

    #[test]
    fn refuses_record_count_that_scanner_would_reject() {
        let path = TempPath::new("record-count-limit");
        let mut journal = Journal::create(&path.0, PROJECT_ID, DIGEST, SCHEMA).unwrap();
        journal.set_next_sequence_for_test(MAX_RECORDS as u64 + 1);
        assert_eq!(
            journal.append(b"{}").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        drop(journal);
        assert!(scan(&path.0, PROJECT_ID, DIGEST, SCHEMA)
            .unwrap()
            .is_empty());
    }

    fn make_test_group(id: u32, name: &str) -> LayerNode {
        LayerNode::Group(LayerGroup {
            properties: LayerNodeProperties::new(id, name, 64).unwrap(),
            children: Vec::new(),
        })
    }

    #[test]
    fn tree_recovery_selects_groups_applies_properties_fill_move_and_remove() {
        let mut doc = Document::new(64);
        doc.layer_tree
            .insert(None, 1, make_test_group(2, "Group"))
            .unwrap();
        doc.layer_tree
            .insert(
                Some(2),
                0,
                LayerNode::Fill(FillLayer {
                    properties: LayerNodeProperties::new(3, "Fill", 64).unwrap(),
                    values: std::collections::HashMap::from([(Channel::BaseColor, [1, 2, 3, 255])]),
                }),
            )
            .unwrap();

        DocumentEvent::LayerSelection {
            set_id: 1,
            layer_id: 2,
        }
        .apply(&mut doc)
        .unwrap();
        assert_eq!(doc.layer_tree.selected_node_id, 2);
        DocumentEvent::LayerTreeNodePropertiesSet {
            set_id: 1,
            node_id: 2,
            name: "Renamed Group".into(),
            visible: false,
            opacity: 0.5,
            blend: BlendMode::Multiply,
            disabled_channels: HashSet::from([Channel::Metallic]),
            procedural_graph: None,
        }
        .apply(&mut doc)
        .unwrap();
        DocumentEvent::LayerTreeFillValueSet {
            set_id: 1,
            node_id: 3,
            channel: Channel::BaseColor,
            value: Some([9, 8, 7, 255]),
        }
        .apply(&mut doc)
        .unwrap();
        let LayerNode::Group(group) = doc.layer_tree.find_node(2).unwrap() else {
            panic!("Group expected")
        };
        assert_eq!(group.properties.name, "Renamed Group");
        assert!(!group.properties.visible);
        let LayerNode::Fill(fill) = doc.layer_tree.find_node(3).unwrap() else {
            panic!("Fill expected")
        };
        assert_eq!(fill.values[&Channel::BaseColor], [9, 8, 7, 255]);
        DocumentEvent::LayerImagePatch {
            set_id: 1,
            layer_id: 2,
            target: LayerImageTarget::Mask,
            x: 6,
            y: 7,
            width: 1,
            height: 1,
            bytes: vec![31],
        }
        .apply(&mut doc)
        .unwrap();
        let LayerNode::Group(group) = doc.layer_tree.find_node(2).unwrap() else {
            panic!("Group expected")
        };
        assert_eq!(group.properties.mask.get_pixel(6, 7).0, [31]);

        DocumentEvent::LayerTreeNodeMove {
            set_id: 1,
            node_id: 3,
            from_parent_id: Some(2),
            from_index: 0,
            to_parent_id: None,
            to_index: 2,
        }
        .apply(&mut doc)
        .unwrap();
        assert_eq!(doc.layer_tree.parent_id(3), Some(None));
        assert_eq!(
            doc.layer_tree.selected_node_id, 2,
            "moving a node preserves selection"
        );
        DocumentEvent::LayerTreeNodeRemove {
            set_id: 1,
            node_id: 3,
            expected_parent_id: None,
            expected_index: 2,
            selected_after_node_id: 1,
        }
        .apply(&mut doc)
        .unwrap();
        assert!(doc.layer_tree.find_node(3).is_none());
        doc.layer_tree.validate(64).unwrap();
    }

    #[test]
    fn recovery_rejects_invalid_cross_parent_move_indices_atomically() {
        let mut doc = Document::new(64);
        doc.layer_tree
            .insert(None, 1, make_test_group(2, "Group"))
            .unwrap();
        doc.layer_tree
            .insert(Some(2), 0, LayerNode::Paint(Layer::new(3, "Nested", 64)))
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

        let invalid_moves = [
            DocumentEvent::LayerTreeNodeMove {
                set_id: 1,
                node_id: 3,
                from_parent_id: Some(2),
                from_index: 0,
                to_parent_id: None,
                to_index: 4, // Root len is 3; len + 1 must be rejected.
            },
            DocumentEvent::LayerTreeNodeMove {
                set_id: 1,
                node_id: 1,
                from_parent_id: None,
                from_index: 0,
                to_parent_id: Some(2),
                to_index: 2, // Group len is 1; len + 1 must be rejected.
            },
            DocumentEvent::LayerTreeNodeMove {
                set_id: 1,
                node_id: 3,
                from_parent_id: Some(2),
                from_index: 0,
                to_parent_id: None,
                to_index: u32::MAX,
            },
        ];

        for event in invalid_moves {
            let before = snapshot(&doc.layer_tree);
            assert!(event.apply(&mut doc).is_err());
            assert_eq!(snapshot(&doc.layer_tree), before);
            doc.layer_tree.validate(64).unwrap();
        }
    }

    #[test]
    fn subtree_transaction_is_atomic_and_incomplete_tail_is_truncated() {
        let mut doc = Document::new(64);
        let mut group = match make_test_group(2, "Restore") {
            LayerNode::Group(group) => group,
            _ => unreachable!(),
        };
        group.children.push(LayerNode::Fill(FillLayer {
            properties: LayerNodeProperties::new(3, "Fill", 64).unwrap(),
            values: std::collections::HashMap::from([(Channel::Roughness, [77, 0, 0, 255])]),
        }));
        let events =
            prepare_layer_tree_subtree_events(&LayerNode::Group(group), 1, None, 1, 2).unwrap();
        let records = records_from_events(&events);
        assert_eq!(committed_record_count(&records).unwrap(), records.len());
        assert_eq!(
            committed_record_count(&records[..records.len() - 1]).unwrap(),
            0
        );

        let mut replayer = MeshRecoveryReplayer::new();
        for event in events.iter().take(events.len() - 1).cloned() {
            replayer.apply_event(&mut doc, event).unwrap();
        }
        assert!(
            doc.layer_tree.find_node(2).is_none(),
            "staged state is not visible before commit"
        );
        assert!(replayer.finish().is_err());
        replayer
            .apply_event(&mut doc, events.last().unwrap().clone())
            .unwrap();
        assert!(doc.layer_tree.find_node(2).is_some());
        assert_eq!(doc.layer_tree.selected_node_id, 2);
        let LayerNode::Fill(fill) = doc.layer_tree.find_node(3).unwrap() else {
            panic!("Fill expected")
        };
        assert_eq!(fill.values[&Channel::Roughness], [77, 0, 0, 255]);
        doc.layer_tree.validate(64).unwrap();
    }

    #[test]
    fn malformed_subtree_chunk_leaves_document_unchanged() {
        let mut doc = Document::new(64);
        let before = doc.layer_tree.all_node_ids();
        let events =
            prepare_layer_tree_subtree_events(&make_test_group(2, "Restore"), 1, None, 1, 2)
                .unwrap();
        let mut replayer = MeshRecoveryReplayer::new();
        replayer.apply_event(&mut doc, events[0].clone()).unwrap();
        let mut chunk = events[1].clone();
        let DocumentEvent::LayerTreeSubtreeChunk { data, .. } = &mut chunk else {
            panic!("Chunk expected")
        };
        *data = "?".into();
        assert!(replayer.apply_event(&mut doc, chunk).is_err());
        assert_eq!(doc.layer_tree.all_node_ids(), before);
        assert_eq!(doc.layer_tree.selected_node_id, 1);
    }

    #[test]
    fn subtree_event_length_accepts_cap_and_rejects_one_byte_over() {
        let event = DocumentEvent::LayerTreeSubtreeBegin {
            transaction_id: Uuid::from_u128(1),
            set_id: 1,
            root_node_id: 2,
            parent_id: None,
            index: 1,
            selected_node_id: 2,
            node_count: 1,
            encoded_len: MAX_LAYER_TREE_SUBTREE_BYTES as u64,
            sha256: [0; 32],
        };
        assert!(validate_tree_subtree_event_bounds(&event).is_ok());
        let too_large = DocumentEvent::LayerTreeSubtreeBegin {
            transaction_id: Uuid::from_u128(1),
            set_id: 1,
            root_node_id: 2,
            parent_id: None,
            index: 1,
            selected_node_id: 2,
            node_count: 1,
            encoded_len: MAX_LAYER_TREE_SUBTREE_BYTES as u64 + 1,
            sha256: [0; 32],
        };
        assert!(validate_tree_subtree_event_bounds(&too_large).is_err());
    }
}

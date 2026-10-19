//! Native .kalium projects are versioned ZIP archives.
//! v4 adds mesh material bindings; v5 adds Detail Normal; v6 adds project identity;
//! v7 adds bounded procedural Roughness graphs; v8 adds AO layer inputs; v9
//! stores a recursive per-set layer tree.
//! Unknown ZIP data is rejected rather than silently discarded during a save.
use crate::document::{
    BlendMode, Channel, Document, Layer, TextureSet, MAX_DECODED_BYTES, MAX_LAYERS, MAX_SIZE,
    MAX_TEXTURE_SETS,
};
use crate::layer_tree::{FillLayer, LayerGroup, LayerNode, LayerNodeProperties, LayerTree};
use crate::mesh::Mesh;
use crate::procedural::ProceduralGraph;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::HashSet,
    fmt,
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
    rc::Rc,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

const MAX_MANIFEST_BYTES: u64 = 48 * 1024 * 1024;
const MAX_IMAGE_ENTRY_OVERHEAD_BYTES: u64 = 4 * 1024 * 1024;
const SEED_DOCUMENT_MASK_BYTES: u64 = 64 * 64;
/// Upper bound for a project file whose bytes are hashed as a recovery baseline.
#[allow(dead_code)] // The S5 journal producer will consume this during app integration.
pub const MAX_PROJECT_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PROJECT_ZIP_CENTRAL_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;
// Each central-directory entry consumes at least a 46-byte fixed header and
// a one-byte nonempty name. Keep the declared count consistent with the
// aggregate central-directory limit, including ZIP64 archives.
const MAX_PROJECT_ZIP_ENTRIES: usize = (MAX_PROJECT_ZIP_CENTRAL_DIRECTORY_BYTES / 47) as usize;
const MAX_PROJECT_ZIP_ENTRY_NAME_BYTES: u16 = 256;
const MAX_PROJECT_ZIP_ENTRY_AUX_BYTES: u16 = 4096;

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    #[serde(default)]
    project_id: Option<uuid::Uuid>,
    mesh: Mesh,
    // Legacy root layout is retained as the active set in v3 and later.
    size: u32,
    selected: usize,
    next_layer_id: u32,
    layers: Vec<LayerManifest>,
    #[serde(default)]
    active_set_id: u32,
    #[serde(default)]
    active_set_name: String,
    #[serde(default)]
    next_set_id: u32,
    #[serde(default)]
    texture_sets: Vec<SetManifest>,
    #[serde(default)]
    material_bindings: Vec<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layer_tree: Option<LayerTreeManifest>,
    #[serde(flatten)]
    extensions: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
struct SetManifest {
    id: u32,
    name: String,
    size: u32,
    selected: usize,
    next_layer_id: u32,
    layers: Vec<LayerManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layer_tree: Option<LayerTreeManifest>,
    #[serde(flatten)]
    extensions: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
struct LayerManifest {
    id: u32,
    name: String,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    channels: Vec<Channel>,
    #[serde(default)]
    disabled_channels: Vec<Channel>,
    #[serde(default)]
    procedural_graph: Option<ProceduralGraph>,
    #[serde(flatten)]
    extensions: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
struct LayerTreeManifest {
    selected_node_id: u32,
    next_node_id: u32,
    roots: Vec<NodeManifest>,
    #[serde(flatten)]
    extensions: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
struct NodePropertiesManifest {
    id: u32,
    name: String,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    #[serde(default)]
    disabled_channels: Vec<Channel>,
    #[serde(flatten)]
    extensions: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum NodeManifest {
    Paint {
        #[serde(flatten)]
        properties: NodePropertiesManifest,
        #[serde(default)]
        channels: Vec<Channel>,
        #[serde(default)]
        procedural_graph: Option<ProceduralGraph>,
    },
    Fill {
        #[serde(flatten)]
        properties: NodePropertiesManifest,
        values: Vec<FillValueManifest>,
    },
    Group {
        #[serde(flatten)]
        properties: NodePropertiesManifest,
        children: Vec<NodeManifest>,
    },
}

#[derive(Serialize, Deserialize)]
struct FillValueManifest {
    channel: Channel,
    value: [u8; 4],
}

fn node_properties_manifest(
    id: u32,
    name: &str,
    visible: bool,
    opacity: f32,
    blend: BlendMode,
    disabled_channels: &std::collections::HashSet<Channel>,
    extensions: &JsonMap<String, JsonValue>,
) -> NodePropertiesManifest {
    NodePropertiesManifest {
        id,
        name: name.to_owned(),
        visible,
        opacity,
        blend,
        disabled_channels: Channel::ALL
            .into_iter()
            .filter(|channel| disabled_channels.contains(channel))
            .collect(),
        extensions: extensions.clone(),
    }
}

fn node_manifest(node: &LayerNode) -> NodeManifest {
    match node {
        LayerNode::Paint(layer) => NodeManifest::Paint {
            properties: node_properties_manifest(
                layer.id,
                &layer.name,
                layer.visible,
                layer.opacity,
                layer.blend,
                &layer.disabled_channels,
                &layer.extensions,
            ),
            channels: Channel::ALL
                .into_iter()
                .filter(|channel| layer.pixels.contains_key(channel))
                .collect(),
            procedural_graph: layer.procedural_graph.clone(),
        },
        LayerNode::Fill(fill) => NodeManifest::Fill {
            properties: node_properties_manifest(
                fill.properties.id,
                &fill.properties.name,
                fill.properties.visible,
                fill.properties.opacity,
                fill.properties.blend,
                &fill.properties.disabled_channels,
                &fill.properties.extensions,
            ),
            values: Channel::ALL
                .into_iter()
                .filter_map(|channel| {
                    fill.values
                        .get(&channel)
                        .copied()
                        .map(|value| FillValueManifest { channel, value })
                })
                .collect(),
        },
        LayerNode::Group(group) => NodeManifest::Group {
            properties: node_properties_manifest(
                group.properties.id,
                &group.properties.name,
                group.properties.visible,
                group.properties.opacity,
                group.properties.blend,
                &group.properties.disabled_channels,
                &group.properties.extensions,
            ),
            children: group.children.iter().map(node_manifest).collect(),
        },
    }
}

fn tree_manifest(tree: &LayerTree, size: u32) -> Result<LayerTreeManifest> {
    tree.validate(size)?;
    validate_tree_extension_budget(tree)?;
    let roots = tree_manifests(&tree.roots, 1)?;
    Ok(LayerTreeManifest {
        selected_node_id: tree.selected_node_id,
        next_node_id: tree.next_node_id,
        roots,
        extensions: tree.extensions.clone(),
    })
}

fn validate_tree_extension_budget(tree: &LayerTree) -> Result<()> {
    let mut total = extension_map_budget(&tree.extensions);
    let mut stack = Vec::new();
    stack.try_reserve(tree.roots.len())?;
    stack.extend(tree.roots.iter());
    while let Some(node) = stack.pop() {
        let (extensions, children) = match node {
            LayerNode::Paint(layer) => (&layer.extensions, None),
            LayerNode::Fill(fill) => (&fill.properties.extensions, None),
            LayerNode::Group(group) => (&group.properties.extensions, Some(&group.children)),
        };
        total = total
            .checked_add(extension_map_budget(extensions))
            .ok_or_else(|| anyhow!("Unknown layer metadata byte count overflow"))?;
        if total as u64 > MAX_MANIFEST_BYTES {
            bail!("Layer tree extensions exceed the 48-MiB metadata budget");
        }
        if let Some(children) = children {
            stack.try_reserve(children.len())?;
            stack.extend(children);
        }
    }
    Ok(())
}

fn extension_map_budget(map: &JsonMap<String, JsonValue>) -> usize {
    map.iter().fold(0usize, |total, (key, value)| {
        total
            .saturating_add(key.len().saturating_add(64))
            .saturating_add(extension_value_budget(value))
    })
}

fn extension_value_budget(value: &JsonValue) -> usize {
    match value {
        JsonValue::Null => 1,
        JsonValue::Bool(_) | JsonValue::Number(_) => 24,
        JsonValue::String(value) => value.len().saturating_add(24),
        JsonValue::Array(values) => values.iter().fold(24usize, |total, value| {
            total
                .saturating_add(24)
                .saturating_add(extension_value_budget(value))
        }),
        JsonValue::Object(values) => 64usize.saturating_add(extension_map_budget(values)),
    }
}

fn tree_manifests(nodes: &[LayerNode], depth: usize) -> Result<Vec<NodeManifest>> {
    if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH {
        bail!("Layer tree exceeds the maximum depth");
    }
    nodes
        .iter()
        .map(|node| {
            let manifest = match node {
                LayerNode::Group(group) => NodeManifest::Group {
                    properties: node_properties_manifest(
                        group.properties.id,
                        &group.properties.name,
                        group.properties.visible,
                        group.properties.opacity,
                        group.properties.blend,
                        &group.properties.disabled_channels,
                        &group.properties.extensions,
                    ),
                    children: tree_manifests(&group.children, depth + 1)?,
                },
                _ => node_manifest(node),
            };
            Ok(manifest)
        })
        .collect()
}

fn prefix(set_id: Option<u32>) -> String {
    match set_id {
        Some(id) => format!("texture_sets/{id}/layers"),
        None => "layers".to_owned(),
    }
}

fn image_name(set_id: Option<u32>, layer_id: u32, channel: Option<Channel>) -> String {
    let suffix = channel.map_or_else(
        || "mask.png".to_owned(),
        |c| format!("{}.png", c.filename()),
    );
    format!("{}/{layer_id}/{suffix}", prefix(set_id))
}

fn write_layer_images<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    opts: SimpleFileOptions,
    set_id: Option<u32>,
    layer: &Layer,
) -> Result<()> {
    writer.start_file(image_name(set_id, layer.id, None), opts)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(layer.mask.clone())
        .write_to(&mut buf, image::ImageFormat::Png)?;
    writer.write_all(&buf.into_inner())?;
    for (channel, pixels) in &layer.pixels {
        writer.start_file(image_name(set_id, layer.id, Some(*channel)), opts)?;
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels.clone())
            .write_to(&mut buf, image::ImageFormat::Png)?;
        writer.write_all(&buf.into_inner())?;
    }
    Ok(())
}

fn write_tree_images<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    opts: SimpleFileOptions,
    set_id: Option<u32>,
    nodes: &[LayerNode],
) -> Result<()> {
    for node in nodes {
        match node {
            LayerNode::Paint(layer) => write_layer_images(writer, opts, set_id, layer)?,
            LayerNode::Fill(fill) => {
                writer.start_file(image_name(set_id, fill.properties.id, None), opts)?;
                let mut buf = Cursor::new(Vec::new());
                image::DynamicImage::ImageLuma8(fill.properties.mask.clone())
                    .write_to(&mut buf, image::ImageFormat::Png)?;
                writer.write_all(&buf.into_inner())?;
            }
            LayerNode::Group(group) => {
                writer.start_file(image_name(set_id, group.properties.id, None), opts)?;
                let mut buf = Cursor::new(Vec::new());
                image::DynamicImage::ImageLuma8(group.properties.mask.clone())
                    .write_to(&mut buf, image::ImageFormat::Png)?;
                writer.write_all(&buf.into_inner())?;
                write_tree_images(writer, opts, set_id, &group.children)?;
            }
        }
    }
    Ok(())
}

pub fn save(doc: &Document, path: &Path) -> Result<()> {
    save_with_ops(
        doc,
        path,
        |temp| File::create(temp),
        File::sync_all,
        replace_project_file,
        sync_parent_directory,
    )
}

/// Save a validated project snapshot at a caller-selected checkpoint archive
/// path using the normal sibling `.tmp` transactional replacement. Callers
/// that also track a primary project path must ensure these destinations are
/// distinct.
pub fn save_checkpoint(doc: &Document, checkpoint_path: &Path) -> Result<()> {
    save(doc, checkpoint_path)
}

/// Load a checkpoint archive through the normal bounded project validator.
pub fn load_checkpoint(checkpoint_path: &Path) -> Result<Document> {
    load(checkpoint_path)
}

/// Compute a bounded SHA-256 digest of the exact saved archive bytes. Recovery
/// journals use it together with the manifest UUID so replay is tied to both a
/// project identity and the specific saved revision.
#[allow(dead_code)] // Kept as a narrow API until the S5 journal producer is wired.
pub fn file_digest(path: &Path) -> Result<[u8; 32]> {
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_PROJECT_ARCHIVE_BYTES {
        bail!("Project archive exceeds the 1-GiB recovery fingerprint limit");
    }
    let mut digest = Sha256::new();
    let mut bytes_read = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let mut reader = file.take(MAX_PROJECT_ARCHIVE_BYTES + 1);
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(count as u64);
        if bytes_read > MAX_PROJECT_ARCHIVE_BYTES {
            bail!("Project archive exceeds the 1-GiB recovery fingerprint limit");
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize().into())
}

/// Derive the recovery identity for pre-v6 archives from the exact bytes on
/// the already-open descriptor. The domain tag keeps this fingerprint
/// separate from other SHA-256 uses in the project/recovery code.
fn legacy_project_id<R: Read + Seek>(reader: &mut R) -> Result<uuid::Uuid> {
    const DOMAIN: &[u8] = b"Kalium legacy project recovery identity v1\0";
    reader.seek(SeekFrom::Start(0))?;
    let mut bounded = reader.take(MAX_PROJECT_ARCHIVE_BYTES + 1);
    let mut digest = Sha256::new();
    digest.update(DOMAIN);
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = bounded.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| anyhow!("Legacy project fingerprint size overflow"))?;
        if total > MAX_PROJECT_ARCHIVE_BYTES {
            bail!("Project archive exceeds the 1-GiB legacy identity limit");
        }
        digest.update(&buffer[..count]);
    }

    let hash = digest.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    // RFC 9562 UUIDv8 custom layout: version nibble 8 and RFC variant 10xx.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes))
}

fn active_manifest_extensions(doc: &Document) -> Result<JsonMap<String, JsonValue>> {
    let mut extensions = doc.extensions.clone();
    for (key, value) in &doc.active_set_extensions {
        if let Some(existing) = extensions.get(key) {
            if existing != value {
                bail!("Conflicting project and active-set extension field '{key}'");
            }
        } else {
            extensions.insert(key.clone(), value.clone());
        }
    }
    Ok(extensions)
}

/// Result of [`load_with_recovery`]. A recovered temporary archive is fully
/// validated by the normal project loader and is returned without replacing
/// the primary file. Callers can therefore prompt before saving it back.
pub enum RecoveryOutcome {
    /// The primary project was valid and took precedence over any temp file.
    Primary(Document),
    /// The primary was missing or invalid; the sibling `.tmp` archive was valid.
    RecoveredTemporary(Document),
}

/// Load a project, falling back to the sibling `<path>.tmp` archive only when
/// the primary is missing or fails full project validation. This function
/// never promotes or overwrites either file. It does not imply power-loss
/// durability. Save syncs the completed temporary archive before replacement
/// and syncs the parent directory on Unix, but Windows directory-entry sync is
/// unavailable through the standard library and filesystem/hardware guarantees
/// still vary.
pub fn load_with_recovery(path: &Path) -> Result<RecoveryOutcome> {
    let mut temp_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid project filename"))?
        .to_os_string();
    temp_name.push(".tmp");
    let tmp = path.with_file_name(temp_name);

    match load(path) {
        Ok(doc) => Ok(RecoveryOutcome::Primary(doc)),
        Err(primary_error) => match load(&tmp) {
            Ok(doc) => Ok(RecoveryOutcome::RecoveredTemporary(doc)),
            Err(temp_error) => Err(anyhow!(
                "Could not load primary project {} ({primary_error:#}) or recover temporary archive {} ({temp_error:#})",
                path.display(),
                tmp.display()
            )),
        },
    }
}

#[cfg(test)]
fn save_with_replace<F>(doc: &Document, path: &Path, replace: F) -> Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    save_with_ops(
        doc,
        path,
        |temp| File::create(temp),
        File::sync_all,
        replace,
        sync_parent_directory,
    )
}

/// Save with injectable filesystem operations so failure ordering can be
/// verified without changing the public save API.
fn save_with_ops<W, C, S, R, D>(
    doc: &Document,
    path: &Path,
    create_temp: C,
    sync_temp: S,
    replace: R,
    sync_parent: D,
) -> Result<()>
where
    W: Write + Seek,
    C: FnOnce(&Path) -> std::io::Result<W>,
    S: FnOnce(&W) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
    D: FnOnce(&Path) -> std::io::Result<()>,
{
    doc.validate()?;
    let manifest = Manifest {
        format: "Kalium Project".into(),
        version: 9,
        project_id: Some(doc.project_id),
        mesh: doc.mesh.clone(),
        size: doc.size,
        selected: 0,
        next_layer_id: doc.layer_tree.next_node_id,
        layers: Vec::new(),
        layer_tree: Some(tree_manifest(&doc.layer_tree, doc.size)?),
        active_set_id: doc.active_set_id,
        active_set_name: doc.active_set_name.clone(),
        // v1-v5 store active-set fields at the manifest root, so root-level
        // extensions belong to the project object itself.
        next_set_id: doc.next_set_id,
        extensions: active_manifest_extensions(doc)?,
        material_bindings: doc.material_bindings.clone(),
        texture_sets: doc
            .texture_sets
            .iter()
            .map(|set| {
                Ok(SetManifest {
                    id: set.id,
                    name: set.name.clone(),
                    size: set.size,
                    selected: 0,
                    next_layer_id: set.layer_tree.next_node_id,
                    layers: Vec::new(),
                    layer_tree: Some(tree_manifest(&set.layer_tree, set.size)?),
                    extensions: set.extensions.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("Project manifest exceeds safety limit");
    }
    let filename = path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid save filename"))?;
    let mut temp_name = filename.to_os_string();
    temp_name.push(".tmp");
    let tmp = path.with_file_name(temp_name);
    let mut temp_created = false;
    let result = (|| -> Result<()> {
        let file = create_temp(&tmp)?;
        temp_created = true;
        let file = write_archive(file, &manifest_bytes, doc)?;
        sync_temp(&file).context("Syncing completed temporary project archive")?;
        Ok(())
    })();
    if let Err(err) = result {
        if temp_created {
            let _ = std::fs::remove_file(&tmp);
        }
        return Err(err);
    }
    if let Err(err) = replace(&tmp, path) {
        // A completed, fully valid temp archive is useful for recovery after a
        // failed replace. Remove only an invalid or incomplete artifact.
        if load(&tmp).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return Err(err).with_context(|| format!("Replacing project {}", path.display()));
    }
    if let Err(error) = sync_parent(path) {
        return Err(PostReplaceSyncError {
            path: path.to_owned(),
            source: error,
        }
        .into());
    }
    Ok(())
}

fn write_archive<W: Write + Seek>(file: W, manifest_bytes: &[u8], doc: &Document) -> Result<W> {
    let mut writer = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer.start_file("manifest.json", opts)?;
    writer.write_all(manifest_bytes)?;
    write_tree_images(&mut writer, opts, None, &doc.layer_tree.roots)?;
    for set in &doc.texture_sets {
        write_tree_images(&mut writer, opts, Some(set.id), &set.layer_tree.roots)?;
    }
    Ok(writer.finish()?)
}

/// This error means the archive replacement already happened. The new primary
/// is visible and valid, but syncing its directory entry failed, so survival
/// across a sudden power loss is uncertain. Callers can downcast this error to
/// distinguish it from a failure that happened before replacement.
#[derive(Debug)]
pub struct PostReplaceSyncError {
    path: std::path::PathBuf,
    source: std::io::Error,
}

impl std::fmt::Display for PostReplaceSyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "project {} was replaced successfully, but syncing its parent directory failed: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for PostReplaceSyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Replace within the same directory so the operation stays on one volume.
/// POSIX rename atomically replaces an existing destination. Windows uses the
/// native replacement API for existing files; a new destination uses a
/// same-volume MoveFileEx operation. Other platforms use their standard rename
/// semantics and therefore inherit those platform's atomicity guarantees.
pub(crate) fn replace_project_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        windows_replace_file(source, destination)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::rename(source, destination)
    }
}

#[cfg(windows)]
fn windows_replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    fn wide(path: &OsStr) -> Vec<u16> {
        path.encode_wide().chain(std::iter::once(0)).collect()
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source_wide = wide(source.as_os_str());
    let destination_wide = wide(destination.as_os_str());
    if destination.exists() {
        // ReplaceFileW is specifically intended to replace an existing file
        // without a destructive remove-then-rename window.
        let succeeded = unsafe {
            ReplaceFileW(
                destination_wide.as_ptr(),
                source_wide.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error());
        }
    } else {
        let succeeded = unsafe {
            MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Directory fsync is available on Unix-like targets. Windows replacement uses
/// the native same-volume APIs, but std does not provide a portable directory
/// handle sync; the primary replacement may therefore lack directory-entry
/// durability across sudden power loss on Windows. Other targets also have no
/// extra directory sync here.
pub(crate) fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn read_entry(zip: &mut ZipArchive<File>, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .with_context(|| format!("Missing {name}"))?;
    if entry.size() > max_bytes {
        bail!("File '{name}' exceeds safety limit");
    }
    let mut buf = Vec::with_capacity(entry.size() as usize);
    (&mut entry).take(max_bytes + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        bail!("File '{name}' expands beyond safety limit");
    }
    Ok(buf)
}

/// Decode a project layer image only after its encoded dimensions match the
/// dimensions declared by the manifest. `into_dimensions` reads decoder
/// metadata without allocating the decoded pixel buffer.
fn decode_layer_image(bytes: &[u8], size: u32, name: &str) -> Result<image::DynamicImage> {
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid expected image dimension for '{name}'");
    }
    let rgba_bytes = (size as u64)
        .checked_mul(size as u64)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("Image dimensions overflow for '{name}'"))?;

    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| format!("Inspecting image '{name}'"))?
        .into_dimensions()
        .with_context(|| format!("Reading dimensions of image '{name}'"))?;
    if dimensions != (size, size) {
        bail!(
            "Image '{name}' dimensions {}×{} do not match project size {}×{}",
            dimensions.0,
            dimensions.1,
            size,
            size
        );
    }

    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| format!("Preparing image '{name}'"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(size);
    limits.max_image_height = Some(size);
    limits.max_alloc = Some(rgba_bytes);
    reader.limits(limits);
    reader
        .decode()
        .with_context(|| format!("Decoding image '{name}'"))
}

/// Account for one encoded entry and one decoder image being live alongside
/// all retained texture pixels. Mask/channel conversions consume the decoded
/// DynamicImage, but can still need an output allocation while converting.
fn validate_image_load_peak(
    retained_bytes: u64,
    largest_texture_size: u32,
    largest_encoded_entry_bytes: u64,
) -> Result<()> {
    let max_decode_scratch = u64::from(largest_texture_size)
        .checked_mul(u64::from(largest_texture_size))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("Decoded image scratch byte count overflow"))?;
    let peak = retained_bytes
        .checked_add(max_decode_scratch)
        .and_then(|bytes| bytes.checked_add(largest_encoded_entry_bytes))
        .and_then(|bytes| bytes.checked_add(SEED_DOCUMENT_MASK_BYTES))
        .ok_or_else(|| anyhow!("Project image-load peak byte count overflow"))?;
    if peak > MAX_DECODED_BYTES {
        bail!("Project exceeds 512-MiB peak image-loading budget");
    }
    Ok(())
}

fn account_layers(
    valid: &mut HashSet<String>,
    total: &mut u64,
    set_id: Option<u32>,
    size: u32,
    layers: &[LayerManifest],
) -> Result<()> {
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid texture dimension");
    }
    if layers.is_empty() || layers.len() > MAX_LAYERS {
        bail!("Invalid layer count");
    }
    let mut ids = HashSet::new();
    for layer in layers {
        if !ids.insert(layer.id) {
            bail!("Duplicate layer id in texture set");
        }
        valid.insert(image_name(set_id, layer.id, None));
        *total = total
            .checked_add((size as u64).pow(2))
            .ok_or_else(|| anyhow!("Decoded texture byte count overflow"))?;
        let mut channels = HashSet::new();
        for &channel in &layer.channels {
            if !channels.insert(channel) {
                bail!("Duplicate channel in layer manifest");
            }
            valid.insert(image_name(set_id, layer.id, Some(channel)));
            *total = total
                .checked_add((size as u64).pow(2) * 4)
                .ok_or_else(|| anyhow!("Decoded texture byte count overflow"))?;
        }
    }
    Ok(())
}

fn account_tree_manifest(
    valid: &mut HashSet<String>,
    total: &mut u64,
    set_id: Option<u32>,
    size: u32,
    tree: &LayerTreeManifest,
) -> Result<()> {
    if !(64..=MAX_SIZE).contains(&size) || tree.roots.is_empty() {
        bail!("Invalid layer tree dimensions or empty roots");
    }
    let mut stack = Vec::new();
    stack.try_reserve(tree.roots.len())?;
    for node in tree.roots.iter().rev() {
        stack.push((node, 1usize));
    }
    let mut ids = HashSet::new();
    let mut node_count = 0usize;
    let mut scheduled_count = tree.roots.len();
    let mut max_id = 0u32;
    let mask_bytes = (size as u64)
        .checked_mul(size as u64)
        .ok_or_else(|| anyhow!("Decoded mask byte count overflow"))?;
    while let Some((node, depth)) = stack.pop() {
        node_count += 1;
        if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH
            || node_count > crate::layer_tree::MAX_LAYER_TREE_NODES
        {
            bail!("Layer tree exceeds its depth or node-count limit");
        }
        let properties = match node {
            NodeManifest::Paint { properties, .. }
            | NodeManifest::Fill { properties, .. }
            | NodeManifest::Group { properties, .. } => properties,
        };
        if !ids.insert(properties.id) {
            bail!("Duplicate node ID in texture set");
        }
        max_id = max_id.max(properties.id);
        if properties.id == 0
            || properties.name.trim().is_empty()
            || properties.name.len() > 1024
            || !properties.opacity.is_finite()
            || !(0.0..=1.0).contains(&properties.opacity)
        {
            bail!("Invalid layer tree node properties");
        }
        let mut disabled = HashSet::new();
        if properties
            .disabled_channels
            .iter()
            .any(|channel| !disabled.insert(*channel))
        {
            bail!("Duplicate disabled channel in layer tree node");
        }
        valid.insert(image_name(set_id, properties.id, None));
        *total = total
            .checked_add(mask_bytes)
            .ok_or_else(|| anyhow!("Decoded texture byte count overflow"))?;
        match node {
            NodeManifest::Paint {
                channels,
                procedural_graph,
                ..
            } => {
                if let Some(graph) = procedural_graph {
                    graph.validate()?;
                }
                if channels.len() > Channel::ALL.len() {
                    bail!("Too many channels in Paint node");
                }
                let mut unique = HashSet::new();
                for &channel in channels {
                    if !unique.insert(channel) {
                        bail!("Duplicate channel in Paint node");
                    }
                    valid.insert(image_name(set_id, properties.id, Some(channel)));
                    *total = total
                        .checked_add(
                            mask_bytes
                                .checked_mul(4)
                                .ok_or_else(|| anyhow!("Decoded channel byte count overflow"))?,
                        )
                        .ok_or_else(|| anyhow!("Decoded texture byte count overflow"))?;
                }
            }
            NodeManifest::Fill { values, .. } => {
                if values.is_empty() || values.len() > Channel::ALL.len() {
                    bail!("Invalid Fill node channel values");
                }
                let mut unique = HashSet::new();
                if values.iter().any(|value| !unique.insert(value.channel)) {
                    bail!("Duplicate Fill node channel value");
                }
            }
            NodeManifest::Group { children, .. } => {
                scheduled_count = scheduled_count
                    .checked_add(children.len())
                    .ok_or_else(|| anyhow!("Layer tree node count overflow"))?;
                if scheduled_count > crate::layer_tree::MAX_LAYER_TREE_NODES {
                    bail!("Layer tree exceeds its node-count limit");
                }
                stack.try_reserve(children.len())?;
                for child in children.iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }
        if *total > MAX_DECODED_BYTES {
            bail!("Project exceeds 512-MiB decoded texture budget");
        }
    }
    if tree.next_node_id == 0 || tree.next_node_id <= max_id {
        bail!("Invalid next layer tree node ID");
    }
    if !ids.contains(&tree.selected_node_id) {
        bail!("Selected layer tree node does not exist");
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct TreePreflightSummary {
    nodes: usize,
    paint_channels: usize,
}

#[derive(Clone)]
struct ExtensionBudget(Rc<Cell<usize>>);

impl ExtensionBudget {
    fn add<E: de::Error>(&self, amount: usize) -> std::result::Result<(), E> {
        let total = self
            .0
            .get()
            .checked_add(amount)
            .ok_or_else(|| E::custom("extension metadata byte count overflow"))?;
        if total as u64 > MAX_MANIFEST_BYTES {
            return Err(E::custom(
                "layer tree extensions exceed the 48-MiB metadata budget",
            ));
        }
        self.0.set(total);
        Ok(())
    }
}

struct ExtensionValueSeed(ExtensionBudget, usize);

impl<'de> DeserializeSeed<'de> for ExtensionValueSeed {
    type Value = ();
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ExtensionValueVisitor(ExtensionBudget, usize);
        impl<'de> Visitor<'de> for ExtensionValueVisitor {
            type Value = ();
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON extension value")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
                self.0.add(24)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
                self.0.add(24)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
                self.0.add(24)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
                self.0.add(24)
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<(), E> {
                self.0.add(value.len().saturating_add(24))
            }
            fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<(), E> {
                self.visit_str(&value)
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<(), E> {
                self.0.add::<E>(1)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
                self.0.add::<E>(1)
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<(), A::Error>
            where
                A: SeqAccess<'de>,
            {
                self.0.add::<A::Error>(24)?;
                while let Some(()) =
                    seq.next_element_seed(ExtensionValueSeed(self.0.clone(), self.1 + 1))?
                {
                    self.0.add::<A::Error>(24)?;
                }
                Ok(())
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<(), A::Error>
            where
                A: MapAccess<'de>,
            {
                self.0.add::<A::Error>(64)?;
                while let Some(key) = map.next_key::<String>()? {
                    self.0.add::<A::Error>(key.len().saturating_add(64))?;
                    map.next_value_seed(ExtensionValueSeed(self.0.clone(), self.1 + 1))?;
                }
                Ok(())
            }
        }
        if self.1 > 64 {
            return Err(de::Error::custom(
                "extension JSON nesting exceeds 64 levels",
            ));
        }
        deserializer.deserialize_any(ExtensionValueVisitor(self.0, self.1))
    }
}

impl TreePreflightSummary {
    fn add(&mut self, other: Self) -> std::result::Result<(), String> {
        self.nodes = self
            .nodes
            .checked_add(other.nodes)
            .ok_or_else(|| "Layer tree node count overflow".to_owned())?;
        self.paint_channels = self
            .paint_channels
            .checked_add(other.paint_channels)
            .ok_or_else(|| "Layer channel count overflow".to_owned())?;
        if self.nodes > crate::layer_tree::MAX_LAYER_TREE_NODES {
            return Err("Layer tree exceeds its node-count limit".to_owned());
        }
        Ok(())
    }

    fn decoded_bytes(self, size: u32) -> std::result::Result<u64, String> {
        let pixels = u64::from(size)
            .checked_mul(u64::from(size))
            .ok_or_else(|| "Decoded texture byte count overflow".to_owned())?;
        let masks = pixels
            .checked_mul(self.nodes as u64)
            .ok_or_else(|| "Decoded mask byte count overflow".to_owned())?;
        let channels = pixels
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_mul(self.paint_channels as u64))
            .ok_or_else(|| "Decoded channel byte count overflow".to_owned())?;
        masks
            .checked_add(channels)
            .ok_or_else(|| "Decoded texture byte count overflow".to_owned())
    }
}

struct CountSequenceSeed {
    maximum: usize,
}

impl<'de> DeserializeSeed<'de> for CountSequenceSeed {
    type Value = usize;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CountVisitor(usize);
        impl<'de> Visitor<'de> for CountVisitor {
            type Value = usize;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "an array with at most {} entries", self.0)
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<usize, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut count = 0usize;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    count += 1;
                    if count > self.0 {
                        return Err(de::Error::custom("array entry limit exceeded"));
                    }
                }
                Ok(count)
            }
        }
        deserializer.deserialize_seq(CountVisitor(self.maximum))
    }
}

struct BoundedTextSeed {
    maximum_bytes: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedTextSeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedTextVisitor(usize);
        impl Visitor<'_> for BoundedTextVisitor {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a string with at most {} UTF-8 bytes", self.0)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<(), E> {
                if value.len() > self.0 {
                    return Err(E::custom("layer node name exceeds 1,024 bytes"));
                }
                Ok(())
            }

            fn visit_borrowed_str<E: de::Error>(
                self,
                value: &'_ str,
            ) -> std::result::Result<(), E> {
                self.visit_str(value)
            }

            fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<(), E> {
                self.visit_str(&value)
            }
        }
        deserializer.deserialize_str(BoundedTextVisitor(self.maximum_bytes))
    }
}

struct NodeListSeed {
    depth: usize,
    allow_empty: bool,
    extension_budget: ExtensionBudget,
}

impl<'de> DeserializeSeed<'de> for NodeListSeed {
    type Value = TreePreflightSummary;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NodeListVisitor(NodeListSeed);
        impl<'de> Visitor<'de> for NodeListVisitor {
            type Value = TreePreflightSummary;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded array of layer nodes")
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut summary = TreePreflightSummary::default();
                while let Some(node) = seq.next_element_seed(NodeSeed {
                    depth: self.0.depth,
                    extension_budget: self.0.extension_budget.clone(),
                })? {
                    summary.add(node).map_err(de::Error::custom)?;
                }
                if !self.0.allow_empty && summary.nodes == 0 {
                    return Err(de::Error::custom("layer tree roots may not be empty"));
                }
                Ok(summary)
            }
        }
        deserializer.deserialize_seq(NodeListVisitor(self))
    }
}

struct NodeSeed {
    depth: usize,
    extension_budget: ExtensionBudget,
}

impl<'de> DeserializeSeed<'de> for NodeSeed {
    type Value = TreePreflightSummary;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NodeVisitor(usize, ExtensionBudget);
        impl<'de> Visitor<'de> for NodeVisitor {
            type Value = TreePreflightSummary;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tagged layer node")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut kind: Option<String> = None;
                let mut has_name = false;
                let mut channels = None;
                let mut fill_values = None;
                let mut children = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" => kind = Some(map.next_value()?),
                        "name" => {
                            map.next_value_seed(BoundedTextSeed {
                                maximum_bytes: 1024,
                            })?;
                            has_name = true;
                        }
                        "channels" => {
                            channels = Some(map.next_value_seed(CountSequenceSeed {
                                maximum: Channel::ALL.len(),
                            })?)
                        }
                        "disabled_channels" => {
                            map.next_value_seed(CountSequenceSeed {
                                maximum: Channel::ALL.len(),
                            })?;
                        }
                        "values" => {
                            fill_values = Some(map.next_value_seed(CountSequenceSeed {
                                maximum: Channel::ALL.len(),
                            })?)
                        }
                        "children" => {
                            children = Some(map.next_value_seed(NodeListSeed {
                                depth: self.0 + 1,
                                allow_empty: true,
                                extension_budget: self.1.clone(),
                            })?)
                        }
                        _ => {
                            if !matches!(
                                key.as_str(),
                                "id" | "name"
                                    | "visible"
                                    | "opacity"
                                    | "blend"
                                    | "disabled_channels"
                            ) {
                                self.1.add::<A::Error>(key.len().saturating_add(64))?;
                                map.next_value_seed(ExtensionValueSeed(self.1.clone(), 0))?;
                            } else {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                    }
                }
                let kind = kind.ok_or_else(|| de::Error::custom("layer node kind is missing"))?;
                if !has_name {
                    return Err(de::Error::custom("layer node name is missing"));
                }
                let mut summary = TreePreflightSummary {
                    nodes: 1,
                    paint_channels: 0,
                };
                match kind.as_str() {
                    "paint" => {
                        if fill_values.is_some() || children.is_some() {
                            return Err(de::Error::custom("invalid Paint node fields"));
                        }
                        summary.paint_channels = channels
                            .ok_or_else(|| de::Error::custom("Paint channels are missing"))?;
                    }
                    "fill" => {
                        if channels.is_some() || children.is_some() {
                            return Err(de::Error::custom("invalid Fill node fields"));
                        }
                        if fill_values
                            .ok_or_else(|| de::Error::custom("Fill values are missing"))?
                            == 0
                        {
                            return Err(de::Error::custom("Fill values may not be empty"));
                        }
                    }
                    "group" => {
                        if channels.is_some() || fill_values.is_some() {
                            return Err(de::Error::custom("invalid Group node fields"));
                        }
                        summary
                            .add(
                                children.ok_or_else(|| {
                                    de::Error::custom("Group children are missing")
                                })?,
                            )
                            .map_err(de::Error::custom)?;
                    }
                    _ => return Err(de::Error::custom("unknown layer node kind")),
                }
                if summary.nodes > crate::layer_tree::MAX_LAYER_TREE_NODES {
                    return Err(de::Error::custom("layer tree node-count limit exceeded"));
                }
                Ok(summary)
            }
        }
        if self.depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH {
            return Err(de::Error::custom("layer tree depth limit exceeded"));
        }
        deserializer.deserialize_map(NodeVisitor(self.depth, self.extension_budget))
    }
}

struct TreeSeed(ExtensionBudget);
impl<'de> DeserializeSeed<'de> for TreeSeed {
    type Value = TreePreflightSummary;
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TreeVisitor(ExtensionBudget);
        impl<'de> Visitor<'de> for TreeVisitor {
            type Value = TreePreflightSummary;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a layer tree object")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut roots = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "roots" {
                        roots = Some(map.next_value_seed(NodeListSeed {
                            depth: 1,
                            allow_empty: false,
                            extension_budget: self.0.clone(),
                        })?);
                    } else {
                        if !matches!(key.as_str(), "selected_node_id" | "next_node_id") {
                            self.0.add::<A::Error>(key.len().saturating_add(64))?;
                            map.next_value_seed(ExtensionValueSeed(self.0.clone(), 0))?;
                        } else {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                roots.ok_or_else(|| de::Error::custom("layer tree roots are missing"))
            }
        }
        deserializer.deserialize_map(TreeVisitor(self.0))
    }
}

struct TextureSetPreflight {
    decoded_bytes: Option<u64>,
}

struct TextureSetSeed(ExtensionBudget);
impl<'de> DeserializeSeed<'de> for TextureSetSeed {
    type Value = TextureSetPreflight;
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TextureSetVisitor(ExtensionBudget);
        impl<'de> Visitor<'de> for TextureSetVisitor {
            type Value = TextureSetPreflight;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a texture set object")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut size = None;
                let mut tree = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "size" => size = Some(map.next_value::<u32>()?),
                        "layer_tree" => tree = Some(map.next_value_seed(TreeSeed(self.0.clone()))?),
                        _ => {
                            self.0.add::<A::Error>(key.len().saturating_add(64))?;
                            map.next_value_seed(ExtensionValueSeed(self.0.clone(), 0))?;
                        }
                    }
                }
                let size = size.ok_or_else(|| de::Error::custom("texture size is missing"))?;
                if !(64..=MAX_SIZE).contains(&size) {
                    return Err(de::Error::custom("invalid texture dimension"));
                }
                let decoded_bytes = tree
                    .map(|tree| tree.decoded_bytes(size).map_err(de::Error::custom))
                    .transpose()?;
                Ok(TextureSetPreflight { decoded_bytes })
            }
        }
        deserializer.deserialize_map(TextureSetVisitor(self.0))
    }
}

struct TextureSetsSeed(ExtensionBudget);
impl<'de> DeserializeSeed<'de> for TextureSetsSeed {
    type Value = Vec<TextureSetPreflight>;
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TextureSetsVisitor(ExtensionBudget);
        impl<'de> Visitor<'de> for TextureSetsVisitor {
            type Value = Vec<TextureSetPreflight>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded array of texture sets")
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut sets = Vec::new();
                while let Some(set) = seq.next_element_seed(TextureSetSeed(self.0.clone()))? {
                    if sets.len() + 1 >= MAX_TEXTURE_SETS {
                        return Err(de::Error::custom("too many texture sets"));
                    }
                    sets.try_reserve(1).map_err(de::Error::custom)?;
                    sets.push(set);
                }
                Ok(sets)
            }
        }
        deserializer.deserialize_seq(TextureSetsVisitor(self.0))
    }
}

struct ManifestPreflight {
    version: u32,
    active_size: Option<u32>,
    active_tree: Option<TreePreflightSummary>,
    sets: Vec<TextureSetPreflight>,
}

struct ManifestPreflightSeed(ExtensionBudget);
impl<'de> DeserializeSeed<'de> for ManifestPreflightSeed {
    type Value = ManifestPreflight;
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ManifestVisitor(ExtensionBudget);
        impl<'de> Visitor<'de> for ManifestVisitor {
            type Value = ManifestPreflight;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a project manifest object")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut version = None;
                let mut active_size = None;
                let mut active_tree = None;
                let mut sets = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" => version = Some(map.next_value::<u32>()?),
                        "size" => active_size = Some(map.next_value::<u32>()?),
                        "layer_tree" => {
                            active_tree = Some(map.next_value_seed(TreeSeed(self.0.clone()))?)
                        }
                        "texture_sets" => {
                            sets = Some(map.next_value_seed(TextureSetsSeed(self.0.clone()))?)
                        }
                        _ => {
                            self.0.add::<A::Error>(key.len().saturating_add(64))?;
                            map.next_value_seed(ExtensionValueSeed(self.0.clone(), 0))?;
                        }
                    }
                }
                Ok(ManifestPreflight {
                    version: version.ok_or_else(|| de::Error::custom("version is missing"))?,
                    active_size,
                    active_tree,
                    sets: sets.unwrap_or_default(),
                })
            }
        }
        deserializer.deserialize_map(ManifestVisitor(self.0))
    }
}

/// Streaming preflight counts retained extension metadata without building a
/// `serde_json::Value` tree. The recursive DTO parser runs only after every v9
/// tree is within the frozen depth/node caps and aggregate decoded-image budget.
fn preflight_manifest_json(bytes: &[u8]) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let preflight = ManifestPreflightSeed(ExtensionBudget(Rc::new(Cell::new(0))))
        .deserialize(&mut deserializer)?;
    deserializer.end()?;
    if preflight.version != 9 {
        return Ok(());
    }
    let size = preflight
        .active_size
        .ok_or_else(|| anyhow!("Version 9 active texture size is missing"))?;
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid texture dimension");
    }
    let active_tree = preflight
        .active_tree
        .ok_or_else(|| anyhow!("Version 9 active layer tree is missing"))?;
    let mut decoded_bytes = active_tree
        .decoded_bytes(size)
        .map_err(|error| anyhow!(error))?;
    for set in preflight.sets {
        if let Some(set_bytes) = set.decoded_bytes {
            decoded_bytes = decoded_bytes
                .checked_add(set_bytes)
                .ok_or_else(|| anyhow!("Decoded texture byte count overflow"))?;
            if decoded_bytes > MAX_DECODED_BYTES {
                bail!("Project exceeds 512-MiB decoded texture budget");
            }
        } else {
            bail!("Version 9 texture set layer tree is missing");
        }
    }
    if decoded_bytes > MAX_DECODED_BYTES {
        bail!("Project exceeds 512-MiB decoded texture budget");
    }
    Ok(())
}

fn validate_layer_metadata(
    size: u32,
    selected: usize,
    next_layer_id: u32,
    set_name: &str,
    layers: &[LayerManifest],
) -> Result<()> {
    if !(64..=MAX_SIZE).contains(&size) {
        bail!("Invalid texture dimension");
    }
    if set_name.trim().is_empty() || set_name.len() > 128 {
        bail!("Invalid texture set name");
    }
    if layers.is_empty() || layers.len() > MAX_LAYERS {
        bail!("Invalid layer count");
    }
    if selected >= layers.len() {
        bail!("Invalid selected layer");
    }
    let mut ids = HashSet::new();
    for layer in layers {
        if layer.name.is_empty() || layer.name.len() > 1024 {
            bail!("Invalid layer name");
        }
        if !layer.opacity.is_finite() || !(0.0..=1.0).contains(&layer.opacity) {
            bail!("Invalid opacity");
        }
        if !ids.insert(layer.id) {
            bail!("Duplicate layer ID");
        }
        if let Some(graph) = &layer.procedural_graph {
            graph.validate()?;
        }
    }
    if next_layer_id <= layers.iter().map(|layer| layer.id).max().unwrap_or(0) {
        bail!("Invalid next layer ID");
    }
    Ok(())
}

fn load_layers(
    zip: &mut ZipArchive<File>,
    set_id: Option<u32>,
    size: u32,
    layers: Vec<LayerManifest>,
) -> Result<Vec<Layer>> {
    let mut result = Vec::with_capacity(layers.len());
    let rgba_bytes = (size as u64).pow(2) * 4;
    for meta in layers {
        let mut layer = Layer::new(meta.id, meta.name, size);
        // The decoded mask replaces Layer::new's opaque default immediately;
        // release that allocation before decoding an untrusted image.
        layer.mask = image::GrayImage::new(0, 0);
        layer.visible = meta.visible;
        layer.opacity = meta.opacity;
        layer.blend = meta.blend;
        layer.disabled_channels = meta.disabled_channels.into_iter().collect();
        layer.procedural_graph = meta.procedural_graph;
        layer.extensions = meta.extensions;
        let bytes = read_entry(
            zip,
            &image_name(set_id, meta.id, None),
            rgba_bytes + MAX_IMAGE_ENTRY_OVERHEAD_BYTES,
        )?;
        let mask_name = image_name(set_id, meta.id, None);
        layer.mask = decode_layer_image(&bytes, size, &mask_name)?.into_luma8();
        if layer.mask.dimensions() != (size, size) {
            bail!("Mask dimensions do not match project");
        }
        for channel in meta.channels {
            let channel_name = image_name(set_id, meta.id, Some(channel));
            let bytes = read_entry(
                zip,
                &channel_name,
                rgba_bytes + MAX_IMAGE_ENTRY_OVERHEAD_BYTES,
            )?;
            let rgba = decode_layer_image(&bytes, size, &channel_name)?.into_rgba8();
            if rgba.dimensions() != (size, size) {
                bail!("Channel dimensions do not match project");
            }
            layer.pixels.insert(channel, rgba);
        }
        result.push(layer);
    }
    Ok(result)
}

fn load_mask(
    zip: &mut ZipArchive<File>,
    set_id: Option<u32>,
    size: u32,
    node_id: u32,
) -> Result<image::GrayImage> {
    let rgba_bytes = (size as u64)
        .checked_mul(size as u64)
        .and_then(|bytes| bytes.checked_mul(4))
        .ok_or_else(|| anyhow!("Decoded mask byte count overflow"))?;
    let name = image_name(set_id, node_id, None);
    let bytes = read_entry(zip, &name, rgba_bytes + MAX_IMAGE_ENTRY_OVERHEAD_BYTES)?;
    let mask = decode_layer_image(&bytes, size, &name)?.into_luma8();
    if mask.dimensions() != (size, size) {
        bail!("Mask dimensions do not match project");
    }
    Ok(mask)
}

fn properties_from_manifest(
    zip: &mut ZipArchive<File>,
    set_id: Option<u32>,
    size: u32,
    manifest: NodePropertiesManifest,
) -> Result<LayerNodeProperties> {
    Ok(LayerNodeProperties {
        id: manifest.id,
        name: manifest.name,
        visible: manifest.visible,
        opacity: manifest.opacity,
        blend: manifest.blend,
        disabled_channels: manifest.disabled_channels.into_iter().collect(),
        mask: load_mask(zip, set_id, size, manifest.id)?,
        extensions: manifest.extensions,
    })
}

fn load_tree_nodes(
    zip: &mut ZipArchive<File>,
    set_id: Option<u32>,
    size: u32,
    nodes: Vec<NodeManifest>,
    depth: usize,
) -> Result<Vec<LayerNode>> {
    if depth > crate::layer_tree::MAX_LAYER_TREE_DEPTH {
        bail!("Layer tree exceeds the maximum depth");
    }
    let mut result = Vec::new();
    result.try_reserve(nodes.len())?;
    for node in nodes {
        match node {
            NodeManifest::Paint {
                properties,
                channels,
                procedural_graph,
            } => {
                let layer_meta = LayerManifest {
                    id: properties.id,
                    name: properties.name,
                    visible: properties.visible,
                    opacity: properties.opacity,
                    blend: properties.blend,
                    channels,
                    disabled_channels: properties.disabled_channels,
                    procedural_graph,
                    extensions: properties.extensions,
                };
                let layer = load_layers(zip, set_id, size, vec![layer_meta])?
                    .pop()
                    .ok_or_else(|| anyhow!("Missing Paint node payload"))?;
                result.push(LayerNode::Paint(layer));
            }
            NodeManifest::Fill { properties, values } => {
                let properties = properties_from_manifest(zip, set_id, size, properties)?;
                let values = values
                    .into_iter()
                    .map(|entry| (entry.channel, entry.value))
                    .collect();
                result.push(LayerNode::Fill(FillLayer { properties, values }));
            }
            NodeManifest::Group {
                properties,
                children,
            } => {
                let properties = properties_from_manifest(zip, set_id, size, properties)?;
                let children = load_tree_nodes(zip, set_id, size, children, depth + 1)?;
                result.push(LayerNode::Group(LayerGroup {
                    properties,
                    children,
                }));
            }
        }
    }
    Ok(result)
}

fn load_tree(
    zip: &mut ZipArchive<File>,
    set_id: Option<u32>,
    size: u32,
    manifest: LayerTreeManifest,
) -> Result<LayerTree> {
    let roots = load_tree_nodes(zip, set_id, size, manifest.roots, 1)?;
    let tree = LayerTree {
        roots,
        selected_node_id: manifest.selected_node_id,
        next_node_id: manifest.next_node_id,
        extensions: manifest.extensions,
    };
    tree.validate(size)?;
    Ok(tree)
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Validate ZIP directory allocation inputs before `ZipArchive::new` trusts
/// the archive's entry count and allocates its file/name tables. This also
/// bounds the aggregate names, comments and extra fields parsed from the
/// central directory.
fn preflight_project_zip_directory<R: Read + Seek>(reader: &mut R) -> Result<usize> {
    const EOCD_SIG: u32 = 0x0605_4b50;
    const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
    const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
    const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
    const EOCD_MIN_BYTES: usize = 22;
    const ZIP64_LOCATOR_BYTES: usize = 20;
    const ZIP64_EOCD_MIN_BYTES: usize = 56;
    const CENTRAL_HEADER_BYTES: u64 = 46;

    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len > MAX_PROJECT_ARCHIVE_BYTES {
        bail!("Project archive exceeds the 1-GiB load limit");
    }
    let tail_len = file_len.min((EOCD_MIN_BYTES + u16::MAX as usize) as u64) as usize;
    if tail_len < EOCD_MIN_BYTES {
        bail!("Project ZIP is too short to contain an end-of-directory record");
    }
    let tail_start = file_len - tail_len as u64;
    reader.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0; tail_len];
    reader.read_exact(&mut tail)?;

    let eocd_index = (0..=tail.len().saturating_sub(EOCD_MIN_BYTES))
        .rev()
        .find(|&index| {
            le_u32(&tail, index) == EOCD_SIG
                && index + EOCD_MIN_BYTES + usize::from(le_u16(&tail, index + 20)) == tail.len()
        })
        .ok_or_else(|| anyhow!("Project ZIP has no valid end-of-directory record"))?;
    let eocd_offset = tail_start + eocd_index as u64;
    let eocd = &tail[eocd_index..];
    let disk_number = le_u16(eocd, 4);
    let central_disk = le_u16(eocd, 6);
    let entries_on_disk = le_u16(eocd, 8);
    let total_entries = le_u16(eocd, 10);
    let classic_size = le_u32(eocd, 12);
    let classic_offset = le_u32(eocd, 16);
    let has_zip64_locator = eocd_offset >= ZIP64_LOCATOR_BYTES as u64 && {
        reader.seek(SeekFrom::Start(eocd_offset - ZIP64_LOCATOR_BYTES as u64))?;
        let mut locator = [0; ZIP64_LOCATOR_BYTES];
        reader.read_exact(&mut locator)?;
        le_u32(&locator, 0) == ZIP64_LOCATOR_SIG
    };

    let (entry_count, central_size, central_offset, central_end) = if has_zip64_locator {
        reader.seek(SeekFrom::Start(eocd_offset - ZIP64_LOCATOR_BYTES as u64))?;
        let mut locator = [0; ZIP64_LOCATOR_BYTES];
        reader.read_exact(&mut locator)?;
        let locator_disk = le_u32(&locator, 4);
        let zip64_offset = le_u64(&locator, 8);
        let disk_count = le_u32(&locator, 16);
        if locator_disk != 0 || disk_count != 1 {
            bail!("Multi-disk project ZIP archives are not supported");
        }

        let locator_offset = eocd_offset - ZIP64_LOCATOR_BYTES as u64;
        if zip64_offset > locator_offset {
            bail!("Invalid ZIP64 project directory offset");
        }
        let footer_search_start = locator_offset
            .saturating_sub((12 + 44 + usize::from(MAX_PROJECT_ZIP_ENTRY_AUX_BYTES)) as u64);
        let footer_search_len = (locator_offset - footer_search_start) as usize;
        reader.seek(SeekFrom::Start(footer_search_start))?;
        let mut footer_search = vec![0; footer_search_len];
        reader.read_exact(&mut footer_search)?;
        let last_footer_start = footer_search
            .len()
            .checked_sub(ZIP64_EOCD_MIN_BYTES)
            .ok_or_else(|| anyhow!("Invalid ZIP64 project directory footer"))?;
        let zip64_index = (0..=last_footer_start)
            .find(|&index| {
                if le_u32(&footer_search, index) != ZIP64_EOCD_SIG {
                    return false;
                }
                let size = le_u64(&footer_search, index + 4);
                size >= 44
                    && size <= 44 + u64::from(MAX_PROJECT_ZIP_ENTRY_AUX_BYTES)
                    && index as u64 + 12 + size == footer_search.len() as u64
            })
            .ok_or_else(|| anyhow!("Invalid ZIP64 project directory footer"))?;
        let zip64_offset_actual = footer_search_start + zip64_index as u64;
        let archive_prefix = zip64_offset_actual
            .checked_sub(zip64_offset)
            .ok_or_else(|| anyhow!("Invalid ZIP64 project directory offset"))?;
        if archive_prefix > 1024 * 1024 {
            bail!("ZIP64 project archive prefix exceeds its safety limit");
        }
        let zip64 = &footer_search[zip64_index..zip64_index + ZIP64_EOCD_MIN_BYTES];
        if le_u32(zip64, 0) != ZIP64_EOCD_SIG {
            bail!("Invalid ZIP64 project directory footer");
        }
        let record_payload_size = le_u64(zip64, 4);
        if record_payload_size < 44
            || record_payload_size > 44 + u64::from(MAX_PROJECT_ZIP_ENTRY_AUX_BYTES)
            || zip64_offset_actual
                .checked_add(12)
                .and_then(|position| position.checked_add(record_payload_size))
                != Some(locator_offset)
        {
            bail!("Invalid ZIP64 project directory size");
        }
        let this_disk = le_u32(zip64, 16);
        let directory_disk = le_u32(zip64, 20);
        let entries_on_disk = le_u64(zip64, 24);
        let total_entries = le_u64(zip64, 32);
        if this_disk != 0 || directory_disk != 0 || entries_on_disk != total_entries {
            bail!("Multi-disk project ZIP archives are not supported");
        }
        (
            usize::try_from(total_entries)
                .map_err(|_| anyhow!("Project ZIP contains too many entries"))?,
            le_u64(zip64, 40),
            le_u64(zip64, 48),
            zip64_offset_actual,
        )
    } else {
        if [
            entries_on_disk == u16::MAX,
            total_entries == u16::MAX,
            classic_size == u32::MAX,
            classic_offset == u32::MAX,
        ]
        .into_iter()
        .any(|is_zip64| is_zip64)
        {
            bail!("ZIP64 project archive is missing its locator");
        }
        if disk_number != 0 || central_disk != 0 || entries_on_disk != total_entries {
            bail!("Multi-disk project ZIP archives are not supported");
        }
        (
            usize::from(total_entries),
            u64::from(classic_size),
            u64::from(classic_offset),
            eocd_offset,
        )
    };

    if entry_count > MAX_PROJECT_ZIP_ENTRIES {
        bail!("Project contains too many ZIP entries");
    }
    if central_size > MAX_PROJECT_ZIP_CENTRAL_DIRECTORY_BYTES {
        bail!("Project ZIP central directory exceeds its safety limit");
    }
    let prefix = central_end
        .checked_sub(central_size)
        .and_then(|end| end.checked_sub(central_offset))
        .ok_or_else(|| anyhow!("Invalid project ZIP central directory offset"))?;
    let central_start = central_offset
        .checked_add(prefix)
        .ok_or_else(|| anyhow!("Invalid project ZIP central directory offset"))?;
    let central_limit = central_start
        .checked_add(central_size)
        .ok_or_else(|| anyhow!("Project ZIP central directory size overflow"))?;
    if central_limit > central_end || central_limit > file_len {
        bail!("Project ZIP central directory exceeds its archive bounds");
    }

    reader.seek(SeekFrom::Start(central_start))?;
    let mut consumed = 0u64;
    for _ in 0..entry_count {
        if consumed
            .checked_add(CENTRAL_HEADER_BYTES)
            .is_none_or(|next| next > central_size)
        {
            bail!("Project ZIP central directory entry exceeds its declared bounds");
        }
        let mut header = [0; CENTRAL_HEADER_BYTES as usize];
        reader.read_exact(&mut header)?;
        if le_u32(&header, 0) != CENTRAL_HEADER_SIG {
            bail!("Invalid project ZIP central directory entry");
        }
        let name_len = le_u16(&header, 28);
        let extra_len = le_u16(&header, 30);
        let comment_len = le_u16(&header, 32);
        if name_len == 0 || name_len > MAX_PROJECT_ZIP_ENTRY_NAME_BYTES {
            bail!("Project ZIP entry name exceeds its safety limit");
        }
        if extra_len > MAX_PROJECT_ZIP_ENTRY_AUX_BYTES
            || comment_len > MAX_PROJECT_ZIP_ENTRY_AUX_BYTES
        {
            bail!("Project ZIP entry metadata exceeds its safety limit");
        }
        let record_size = CENTRAL_HEADER_BYTES
            + u64::from(name_len)
            + u64::from(extra_len)
            + u64::from(comment_len);
        consumed = consumed
            .checked_add(record_size)
            .ok_or_else(|| anyhow!("Project ZIP central directory size overflow"))?;
        if consumed > central_size {
            bail!("Project ZIP central directory entry exceeds its declared bounds");
        }
        reader.seek(SeekFrom::Current(i64::from(
            name_len + extra_len + comment_len,
        )))?;
    }

    Ok(entry_count)
}

pub fn load(path: &Path) -> Result<Document> {
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_PROJECT_ARCHIVE_BYTES {
        bail!("Project archive exceeds the 1-GiB load limit");
    }
    let mut file = file;
    let expected_entry_count = preflight_project_zip_directory(&mut file)?;
    file.seek(SeekFrom::Start(0))?;
    let mut zip = ZipArchive::new(file).context("Not a valid Kalium project ZIP")?;
    if zip.len() != expected_entry_count {
        bail!("Project ZIP directory changed while it was being opened");
    }
    let manifest_bytes = read_entry(&mut zip, "manifest.json", MAX_MANIFEST_BYTES)?;
    preflight_manifest_json(&manifest_bytes)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.format != "Kalium Project" || !matches!(manifest.version, 1..=9) {
        bail!("Unsupported Kalium project version");
    }
    if (manifest.version == 9) != manifest.layer_tree.is_some()
        || (manifest.version < 9
            && manifest
                .texture_sets
                .iter()
                .any(|set| set.layer_tree.is_some()))
    {
        bail!("Project layer-tree data does not match its manifest version");
    }
    if manifest.version >= 6
        && manifest
            .project_id
            .is_none_or(|project_id| project_id.is_nil())
    {
        bail!("Project identity is missing or invalid");
    }
    if manifest.version < 6 && manifest.project_id.is_some() {
        bail!("Legacy project unexpectedly contains project identity metadata");
    }
    if manifest.version < 4 && !manifest.material_bindings.is_empty() {
        bail!("Legacy project unexpectedly contains material binding metadata");
    }
    if manifest.version >= 4 && manifest.material_bindings.len() != manifest.mesh.material_count() {
        bail!("Material binding count does not match mesh");
    }
    if manifest.version < 3 && (!manifest.texture_sets.is_empty() || manifest.active_set_id != 0) {
        bail!("Legacy project contains unexpected texture set metadata");
    }
    if manifest.version < 7
        && (manifest
            .layers
            .iter()
            .any(|layer| layer.procedural_graph.is_some())
            || manifest
                .texture_sets
                .iter()
                .flat_map(|set| &set.layers)
                .any(|layer| layer.procedural_graph.is_some()))
    {
        bail!("Legacy project unexpectedly contains procedural graph data");
    }
    if manifest.version < 8
        && manifest
            .layers
            .iter()
            .chain(manifest.texture_sets.iter().flat_map(|set| &set.layers))
            .filter_map(|layer| layer.procedural_graph.as_ref())
            .any(ProceduralGraph::has_occlusion_input)
    {
        bail!("Legacy project unexpectedly contains AO procedural graph inputs");
    }
    manifest.mesh.validate()?;
    if manifest.version < 9 {
        validate_layer_metadata(
            manifest.size,
            manifest.selected,
            manifest.next_layer_id,
            if manifest.version >= 3 {
                &manifest.active_set_name
            } else {
                "Default"
            },
            &manifest.layers,
        )?;
        if manifest.version >= 3 {
            for set in &manifest.texture_sets {
                validate_layer_metadata(
                    set.size,
                    set.selected,
                    set.next_layer_id,
                    &set.name,
                    &set.layers,
                )?;
            }
        }
    } else {
        if !manifest.layers.is_empty()
            || manifest
                .texture_sets
                .iter()
                .any(|set| !set.layers.is_empty())
        {
            bail!("Version 9 project contains duplicate legacy layer storage");
        }
        if manifest.active_set_name.trim().is_empty() || manifest.active_set_name.len() > 128 {
            bail!("Invalid active texture set name");
        }
        for set in &manifest.texture_sets {
            if set.name.trim().is_empty() || set.name.len() > 128 {
                bail!("Invalid texture set name");
            }
        }
    }
    for id in manifest.material_bindings.iter().flatten() {
        if *id != manifest.active_set_id && !manifest.texture_sets.iter().any(|set| set.id == *id) {
            bail!("Material binding references missing texture set {id}");
        }
    }
    let mut valid = HashSet::from(["manifest.json".to_owned()]);
    let mut decoded_bytes = 0;
    if manifest.version >= 9 {
        account_tree_manifest(
            &mut valid,
            &mut decoded_bytes,
            None,
            manifest.size,
            manifest.layer_tree.as_ref().unwrap(),
        )?;
    } else {
        account_layers(
            &mut valid,
            &mut decoded_bytes,
            None,
            manifest.size,
            &manifest.layers,
        )?;
    }
    if manifest.texture_sets.len() >= MAX_TEXTURE_SETS {
        bail!("Too many texture sets");
    }
    let mut set_ids = HashSet::new();
    if manifest.version >= 3 {
        if manifest.active_set_id == 0 || manifest.next_set_id <= manifest.active_set_id {
            bail!("Invalid active texture set identifier");
        }
        set_ids.insert(manifest.active_set_id);
        for set in &manifest.texture_sets {
            if set.id == 0 || set.id >= manifest.next_set_id || !set_ids.insert(set.id) {
                bail!("Duplicate/invalid texture set identifier");
            }
            if manifest.version >= 9 {
                account_tree_manifest(
                    &mut valid,
                    &mut decoded_bytes,
                    Some(set.id),
                    set.size,
                    set.layer_tree.as_ref().unwrap(),
                )?;
            } else {
                account_layers(
                    &mut valid,
                    &mut decoded_bytes,
                    Some(set.id),
                    set.size,
                    &set.layers,
                )?;
            }
        }
    }
    if decoded_bytes > MAX_DECODED_BYTES {
        bail!("Project exceeds 512-MiB decoded texture budget");
    }
    let largest_texture_size = manifest
        .texture_sets
        .iter()
        .map(|set| set.size)
        .fold(manifest.size, u32::max);
    let largest_rgba_bytes = u64::from(largest_texture_size)
        .checked_mul(u64::from(largest_texture_size))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("Decoded texture byte count overflow"))?;
    let mut seen = HashSet::new();
    let mut largest_encoded_image_bytes = 0;
    for idx in 0..zip.len() {
        let entry = zip.by_index(idx)?;
        let entry_name = entry.name()?.into_owned();
        if !seen.insert(entry_name.clone()) {
            bail!(
                "Duplicate ZIP entry '{}'; refusing ambiguous input",
                entry_name
            );
        }
        if entry.is_dir() {
            let name = entry_name.trim_end_matches('/');
            let is_parent = valid
                .iter()
                .any(|path| path.starts_with(&format!("{name}/")));
            if !is_parent {
                bail!("Unexpected ZIP directory '{}'", entry_name);
            }
        } else {
            if !valid.contains(entry_name.as_str()) {
                bail!(
                    "Unknown archive entry '{}'; refusing lossy import",
                    entry_name
                );
            }
            if entry_name != "manifest.json" {
                let max_entry_bytes = largest_rgba_bytes + MAX_IMAGE_ENTRY_OVERHEAD_BYTES;
                if entry.size() > max_entry_bytes {
                    bail!("File '{entry_name}' exceeds safety limit");
                }
                largest_encoded_image_bytes = largest_encoded_image_bytes.max(entry.size());
            }
        }
    }
    validate_image_load_peak(
        decoded_bytes,
        largest_texture_size,
        largest_encoded_image_bytes,
    )?;
    // Project data replaces this seed tree below. Keep its temporary mask at
    // the minimum size so it does not duplicate a large active-set allocation.
    let mut doc = Document::new(64);
    if let Some(project_id) = manifest.project_id {
        doc.project_id = project_id;
    }
    doc.mesh = manifest.mesh;
    doc.material_bindings = if manifest.version >= 4 {
        manifest.material_bindings
    } else {
        vec![None; doc.mesh.material_count()]
    };
    doc.size = manifest.size;
    let mut project_extensions = manifest.extensions;
    // This namespaced option was historically emitted at the active Texture
    // Set's root manifest level; recover it into the selected set's extension
    // map so switching sets does not turn it into project-global state.
    if let Some(ao_settings) = project_extensions.remove(crate::bake::AO_SETTINGS_EXTENSION_KEY) {
        doc.active_set_extensions.insert(
            crate::bake::AO_SETTINGS_EXTENSION_KEY.to_owned(),
            ao_settings,
        );
    }
    doc.extensions = project_extensions;
    if manifest.version >= 9 {
        doc.layer_tree = load_tree(
            &mut zip,
            None,
            manifest.size,
            manifest
                .layer_tree
                .ok_or_else(|| anyhow!("Version 9 project is missing its layer tree"))?,
        )?;
    } else {
        let layers = load_layers(&mut zip, None, manifest.size, manifest.layers)?;
        doc.layer_tree =
            LayerTree::from_flat_owned(layers, manifest.selected, manifest.next_layer_id)?;
    }
    if manifest.version >= 3 {
        doc.active_set_id = manifest.active_set_id;
        doc.active_set_name = manifest.active_set_name;
        doc.next_set_id = manifest.next_set_id;
        for set in manifest.texture_sets {
            let layer_tree = if manifest.version >= 9 {
                load_tree(
                    &mut zip,
                    Some(set.id),
                    set.size,
                    set.layer_tree
                        .ok_or_else(|| anyhow!("Version 9 texture set is missing its tree"))?,
                )?
            } else {
                let layers = load_layers(&mut zip, Some(set.id), set.size, set.layers)?;
                LayerTree::from_flat_owned(layers, set.selected, set.next_layer_id)?
            };
            doc.texture_sets.push(TextureSet {
                id: set.id,
                name: set.name,
                size: set.size,
                layer_tree,
                extensions: set.extensions,
            });
        }
    }
    doc.validate()?;
    if manifest.version < 6 {
        let mut archive_file = zip.into_inner();
        doc.project_id = legacy_project_id(&mut archive_file)?;
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{export, paint::Brush, uv::UvCoverage};
    use image::{Luma, Rgba};
    use std::{cell::Cell, path::PathBuf, rc::Rc};

    struct Cleanup(Vec<PathBuf>);

    fn central_directory_entry(name_len: u16) -> Vec<u8> {
        let mut entry = vec![0; 46 + usize::from(name_len)];
        entry[..4].copy_from_slice(&0x0201_4b50u32.to_le_bytes());
        entry[28..30].copy_from_slice(&name_len.to_le_bytes());
        entry[46] = b'x';
        entry
    }

    fn classic_zip_directory(entries: &[Vec<u8>], declared_entries: u16) -> Vec<u8> {
        let central_size: usize = entries.iter().map(Vec::len).sum();
        let mut bytes: Vec<u8> = entries.iter().flatten().copied().collect();
        bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // disk number
        bytes.extend_from_slice(&0u16.to_le_bytes()); // directory disk
        bytes.extend_from_slice(&declared_entries.to_le_bytes());
        bytes.extend_from_slice(&declared_entries.to_le_bytes());
        bytes.extend_from_slice(&(central_size as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment length
        bytes
    }

    fn assert_preflight_rejects_without_panicking(bytes: &[u8], label: &str) {
        // The synthetic archive is only a few hundred bytes. Keep the mutation
        // corpus small and in memory so malformed offsets cannot trigger large
        // test allocations or depend on filesystem/ZIP writer behavior.
        assert!(bytes.len() < 1024, "{label} fixture unexpectedly grew");
        let result =
            std::panic::catch_unwind(|| preflight_project_zip_directory(&mut Cursor::new(bytes)));
        let result = result.unwrap_or_else(|_| panic!("preflight panicked for {label}"));
        assert!(result.is_err(), "preflight accepted {label}");
    }

    fn zip64_directory_footer(central: &[u8], entries: u64, declared_size: u64) -> Vec<u8> {
        let zip64_offset = central.len() as u64;
        let mut bytes = central.to_vec();
        bytes.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&44u64.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // version made by
        bytes.extend_from_slice(&45u16.to_le_bytes()); // version needed
        bytes.extend_from_slice(&0u32.to_le_bytes()); // disk number
        bytes.extend_from_slice(&0u32.to_le_bytes()); // directory disk
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&declared_size.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // directory offset
        bytes.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // disk containing footer
        bytes.extend_from_slice(&zip64_offset.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // disks
        bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    #[test]
    fn zip_preflight_accepts_entry_count_limit_before_archive_construction() {
        let header = central_directory_entry(1);
        let entries = vec![header; MAX_PROJECT_ZIP_ENTRIES];
        let central_size = entries.iter().map(Vec::len).sum::<usize>() as u64;
        let central = entries.into_iter().flatten().collect::<Vec<_>>();
        let bytes = zip64_directory_footer(&central, MAX_PROJECT_ZIP_ENTRIES as u64, central_size);
        let mut reader = Cursor::new(bytes);
        let entries = preflight_project_zip_directory(&mut reader).unwrap();
        assert_eq!(entries, MAX_PROJECT_ZIP_ENTRIES);
    }

    #[test]
    fn zip_preflight_rejects_over_limit_declared_count_before_reading_entries() {
        let bytes = zip64_directory_footer(&[], (MAX_PROJECT_ZIP_ENTRIES + 1) as u64, 0);
        let mut reader = Cursor::new(bytes);
        let error = preflight_project_zip_directory(&mut reader).unwrap_err();
        assert!(
            error.to_string().contains("too many ZIP entries"),
            "{error:#}"
        );
    }

    #[test]
    fn project_load_preflights_declared_entry_count_before_ziparchive_construction() {
        let path = path("declared-zip-entry-count");
        let cleanup = Cleanup(vec![path.clone()]);
        let bytes = zip64_directory_footer(&[], (MAX_PROJECT_ZIP_ENTRIES + 1) as u64, 0);
        std::fs::write(&path, bytes).unwrap();
        let error = match load(&path) {
            Ok(_) => panic!("over-limit ZIP entry count should be rejected"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("too many ZIP entries"),
            "{error:#}"
        );
        drop(cleanup);
    }

    #[test]
    fn zip64_preflight_rejects_over_limit_count_before_archive_construction() {
        let bytes = zip64_directory_footer(&[], (MAX_PROJECT_ZIP_ENTRIES + 1) as u64, 0);
        let mut reader = Cursor::new(bytes);
        let error = preflight_project_zip_directory(&mut reader).unwrap_err();
        assert!(
            error.to_string().contains("too many ZIP entries"),
            "{error:#}"
        );
    }

    #[test]
    fn zip64_preflight_accepts_bounded_directory() {
        let entry = central_directory_entry(1);
        let bytes = zip64_directory_footer(&entry, 1, entry.len() as u64);
        let mut reader = Cursor::new(bytes);
        let entries = preflight_project_zip_directory(&mut reader).unwrap();
        assert_eq!(entries, 1);
    }

    #[test]
    fn zip64_preflight_accepts_small_prepended_stub() {
        let entry = central_directory_entry(1);
        let mut bytes = zip64_directory_footer(&entry, 1, entry.len() as u64);
        let zip64_relative_offset = entry.len() as u64;
        let prefix = b"KALIUM!";
        bytes.splice(0..0, prefix.iter().copied());
        let locator_offset = prefix.len() + entry.len() + 56;
        bytes[locator_offset + 8..locator_offset + 16]
            .copy_from_slice(&zip64_relative_offset.to_le_bytes());
        let mut reader = Cursor::new(bytes);
        assert_eq!(preflight_project_zip_directory(&mut reader).unwrap(), 1);
    }

    #[test]
    fn zip_preflight_rejects_oversized_directory_and_entry_names() {
        let mut bytes = classic_zip_directory(&[], 0);
        let size_offset = bytes.len() - 10;
        bytes[size_offset..size_offset + 4]
            .copy_from_slice(&((MAX_PROJECT_ZIP_CENTRAL_DIRECTORY_BYTES + 1) as u32).to_le_bytes());
        let mut reader = Cursor::new(bytes);
        let error = preflight_project_zip_directory(&mut reader).unwrap_err();
        assert!(
            error.to_string().contains("central directory exceeds"),
            "{error:#}"
        );

        let oversized_name = central_directory_entry(MAX_PROJECT_ZIP_ENTRY_NAME_BYTES + 1);
        let bytes = classic_zip_directory(&[oversized_name], 1);
        let mut reader = Cursor::new(bytes);
        let error = preflight_project_zip_directory(&mut reader).unwrap_err();
        assert!(
            error.to_string().contains("entry name exceeds"),
            "{error:#}"
        );
    }

    #[test]
    fn zip_preflight_rejects_deterministic_directory_mutations_without_panicking() {
        let valid_entry = central_directory_entry(1);

        for (label, bytes) in [
            ("empty archive", &[][..]),
            ("one-byte archive", &[0x50][..]),
            ("three-byte archive", &[0x50, 0x4b, 0x05][..]),
        ] {
            assert_preflight_rejects_without_panicking(bytes, label);
        }

        let mut bad_signature = valid_entry.clone();
        bad_signature[0] ^= 0x80;
        assert_preflight_rejects_without_panicking(
            &classic_zip_directory(&[bad_signature], 1),
            "central-directory signature mutation",
        );

        let mut zero_name = valid_entry.clone();
        zero_name[28..30].copy_from_slice(&0u16.to_le_bytes());
        assert_preflight_rejects_without_panicking(
            &classic_zip_directory(&[zero_name], 1),
            "zero-length entry name",
        );

        // The name length claims two bytes but the central-directory size only
        // contains one. This catches truncated variable-length fields.
        let mut truncated_name = valid_entry.clone();
        truncated_name[28..30].copy_from_slice(&2u16.to_le_bytes());
        assert_preflight_rejects_without_panicking(
            &classic_zip_directory(&[truncated_name], 1),
            "truncated central-directory name",
        );

        let mut bad_offset = classic_zip_directory(std::slice::from_ref(&valid_entry), 1);
        let eocd = bad_offset.len() - 22;
        bad_offset[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_preflight_rejects_without_panicking(
            &bad_offset,
            "central-directory offset overflow",
        );

        let mut multi_disk = classic_zip_directory(&[valid_entry], 1);
        let eocd = multi_disk.len() - 22;
        multi_disk[eocd + 4..eocd + 6].copy_from_slice(&1u16.to_le_bytes());
        assert_preflight_rejects_without_panicking(&multi_disk, "multi-disk EOCD mutation");
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            for path in &self.0 {
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(path);
                } else {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut encoded = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, Rgba([1, 2, 3, 255]))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let mut bytes = encoded.into_inner();
        // Keep the PNG signature, IHDR, and next chunk header. The dimensions are
        // readable, but attempting to decode pixels fails because IDAT is
        // absent. This proves mismatches are rejected before pixel decoding.
        bytes.truncate(41);
        bytes
    }

    #[test]
    fn rejects_manifest_dimension_mismatch_before_decoding_pixels() {
        let bytes = png_header(65, 64);
        let err = decode_layer_image(&bytes, 64, "layers/1/BaseColor.png").unwrap_err();
        assert!(err.to_string().contains("dimensions 65×64"), "{err:#}");
        assert!(err.to_string().contains("project size 64×64"));
    }

    #[test]
    fn rejects_declared_dimensions_above_project_limit_before_decoding() {
        let bytes = png_header(MAX_SIZE + 1, 64);
        let err = decode_layer_image(&bytes, 64, "layers/1/mask.png").unwrap_err();
        assert!(err.to_string().contains("dimensions 4097×64"), "{err:#}");
        assert!(err.to_string().contains("project size 64×64"));
    }

    fn path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kalium-{label}-{}-{}.kalium",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ))
    }

    fn set_marker(doc: &mut Document, value: u8) {
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, size)
            .put_pixel(3, 5, Rgba([value, 0, 0, 255]));
    }

    fn marker(doc: &Document) -> [u8; 4] {
        doc.composite(Channel::BaseColor).get_pixel(3, 5).0
    }

    fn temp_path(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap().to_os_string();
        name.push(".tmp");
        path.with_file_name(name)
    }

    struct FailingWriter {
        file: File,
        bytes_until_failure: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes_until_failure == 0 {
                return Err(std::io::Error::other("injected archive write failure"));
            }
            let allowed = bytes.len().min(self.bytes_until_failure);
            let written = self.file.write(&bytes[..allowed])?;
            self.bytes_until_failure -= written;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    impl Seek for FailingWriter {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            self.file.seek(position)
        }
    }

    struct CountingWriter {
        file: File,
        bytes_written: Rc<Cell<usize>>,
    }

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let written = self.file.write(bytes)?;
            self.bytes_written
                .set(self.bytes_written.get().saturating_add(written));
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }

    impl Seek for CountingWriter {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            self.file.seek(position)
        }
    }

    fn rewrite_manifest(path: &Path, mut update: impl FnMut(&mut serde_json::Value)) {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == "manifest.json" {
                let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                update(&mut manifest);
                bytes = serde_json::to_vec_pretty(&manifest).unwrap();
            }
            entries.push((name, bytes));
        }
        drop(archive);

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        let rewritten = writer.finish().unwrap().into_inner();
        std::fs::write(path, rewritten).unwrap();
    }

    fn rewrite_as_legacy_version(path: &Path, version: u32) {
        fn flatten_tree(tree: &mut serde_json::Value) {
            let roots = tree["roots"].as_array_mut().unwrap();
            fn visit(nodes: &mut [serde_json::Value]) {
                for node in nodes {
                    let kind = node["kind"].as_str().unwrap().to_owned();
                    if kind == "group" {
                        visit(node["children"].as_array_mut().unwrap());
                        panic!("legacy format cannot encode groups");
                    }
                    node.as_object_mut().unwrap().remove("kind");
                    if kind == "fill" {
                        panic!("legacy format cannot encode fills");
                    }
                }
            }
            visit(roots);
        }
        rewrite_manifest(path, |manifest| {
            manifest["version"] = serde_json::json!(version);
            for prefix in ["", "texture_sets"] {
                if prefix.is_empty() {
                    let tree = manifest
                        .as_object_mut()
                        .unwrap()
                        .remove("layer_tree")
                        .unwrap();
                    let selected_id = tree["selected_node_id"].as_u64().unwrap() as u32;
                    let next_id = tree["next_node_id"].clone();
                    let mut tree = tree;
                    flatten_tree(&mut tree);
                    let layers = tree["roots"].clone();
                    let selected = layers
                        .as_array()
                        .unwrap()
                        .iter()
                        .position(|layer| layer["id"] == selected_id)
                        .unwrap();
                    manifest["layers"] = layers;
                    manifest["selected"] = serde_json::json!(selected);
                    manifest["next_layer_id"] = next_id;
                } else {
                    for set in manifest["texture_sets"].as_array_mut().unwrap() {
                        let tree = set.as_object_mut().unwrap().remove("layer_tree").unwrap();
                        let selected_id = tree["selected_node_id"].as_u64().unwrap() as u32;
                        let next_id = tree["next_node_id"].clone();
                        let mut tree = tree;
                        flatten_tree(&mut tree);
                        let layers = tree["roots"].clone();
                        let selected = layers
                            .as_array()
                            .unwrap()
                            .iter()
                            .position(|layer| layer["id"] == selected_id)
                            .unwrap();
                        set["layers"] = layers;
                        set["selected"] = serde_json::json!(selected);
                        set["next_layer_id"] = next_id;
                    }
                }
            }
            let object = manifest.as_object_mut().unwrap();
            if version < 3 {
                for key in [
                    "active_set_id",
                    "active_set_name",
                    "next_set_id",
                    "texture_sets",
                ] {
                    object.remove(key);
                }
            }
            if version < 4 {
                object.remove("material_bindings");
            }
            if version < 6 {
                object.remove("project_id");
            }
        });
    }

    fn rewrite_manifest_and_entry(
        path: &Path,
        mut update: impl FnMut(&mut serde_json::Value),
        replacement: (&str, &[u8]),
    ) {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == "manifest.json" {
                let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                update(&mut manifest);
                bytes = serde_json::to_vec_pretty(&manifest).unwrap();
            }
            if name == replacement.0 {
                bytes = replacement.1.to_vec();
            }
            entries.push((name, bytes));
        }
        drop(archive);

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        let rewritten = writer.finish().unwrap().into_inner();
        std::fs::write(path, rewritten).unwrap();
    }

    fn add_archive_entry(path: &Path, name: &str, bytes: &[u8]) {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let entry_name = entry.name().unwrap().into_owned();
            let mut entry_bytes = Vec::new();
            entry.read_to_end(&mut entry_bytes).unwrap();
            entries.push((entry_name, entry_bytes));
        }
        drop(archive);

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (entry_name, entry_bytes) in entries {
            writer.start_file(entry_name, options).unwrap();
            writer.write_all(&entry_bytes).unwrap();
        }
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
        let rewritten = writer.finish().unwrap().into_inner();
        std::fs::write(path, rewritten).unwrap();
    }

    fn duplicate_central_directory_entry(path: &Path, name: &str) {
        const EOCD_SIG: u32 = 0x0605_4b50;
        const CENTRAL_SIG: u32 = 0x0201_4b50;
        const EOCD_BYTES: usize = 22;
        const CENTRAL_HEADER_BYTES: usize = 46;

        let bytes = std::fs::read(path).unwrap();
        let eocd = (0..=bytes.len().saturating_sub(EOCD_BYTES))
            .rev()
            .find(|&index| le_u32(&bytes, index) == EOCD_SIG)
            .expect("writer should produce an EOCD record");
        let entry_count = le_u16(&bytes, eocd + 10);
        let central_size = le_u32(&bytes, eocd + 12) as usize;
        let central_start = le_u32(&bytes, eocd + 16) as usize;
        let central_end = central_start.checked_add(central_size).unwrap();
        assert!(central_end <= eocd);

        let mut cursor = central_start;
        let mut duplicate = None;
        while cursor < central_end {
            assert_eq!(le_u32(&bytes, cursor), CENTRAL_SIG);
            let name_len = usize::from(le_u16(&bytes, cursor + 28));
            let extra_len = usize::from(le_u16(&bytes, cursor + 30));
            let comment_len = usize::from(le_u16(&bytes, cursor + 32));
            let record_len = CENTRAL_HEADER_BYTES
                .checked_add(name_len)
                .and_then(|len| len.checked_add(extra_len))
                .and_then(|len| len.checked_add(comment_len))
                .expect("central entry length should not overflow");
            let record_end = cursor.checked_add(record_len).unwrap();
            assert!(record_end <= central_end);
            if &bytes[cursor + CENTRAL_HEADER_BYTES..cursor + CENTRAL_HEADER_BYTES + name_len]
                == name.as_bytes()
            {
                duplicate = Some(bytes[cursor..record_end].to_vec());
                break;
            }
            cursor = record_end;
        }
        let duplicate = duplicate.expect("requested ZIP entry should exist");
        let mut rewritten = Vec::with_capacity(bytes.len() + duplicate.len());
        rewritten.extend_from_slice(&bytes[..central_end]);
        rewritten.extend_from_slice(&duplicate);
        rewritten.extend_from_slice(&bytes[central_end..]);
        let new_eocd = eocd + duplicate.len();
        let new_count = entry_count.checked_add(1).expect("test archive count fits");
        rewritten[new_eocd + 8..new_eocd + 10].copy_from_slice(&new_count.to_le_bytes());
        rewritten[new_eocd + 10..new_eocd + 12].copy_from_slice(&new_count.to_le_bytes());
        let new_size = central_size
            .checked_add(duplicate.len())
            .expect("test archive directory size fits");
        rewritten[new_eocd + 12..new_eocd + 16].copy_from_slice(&(new_size as u32).to_le_bytes());
        std::fs::write(path, rewritten).unwrap();
    }

    fn saved_manifest(path: &Path) -> serde_json::Value {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let bytes = read_entry(&mut archive, "manifest.json", MAX_MANIFEST_BYTES).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn project_round_trip_keeps_editable_layers() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(12, 12, Luma([70]));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(11, 10, Rgba([12, 34, 56, 200]));
        doc.selected_layer_mut()
            .unwrap()
            .disabled_channels
            .insert(Channel::BaseColor);
        let path = path("roundtrip");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.layer_tree.ordered_paint_leaf_ids().len(), 2);
        assert!(loaded
            .selected_layer()
            .unwrap()
            .disabled_channels
            .contains(&Channel::BaseColor));
        let layer = loaded.layer_tree.paint_layer(2).unwrap();
        assert_eq!(layer.mask.get_pixel(12, 12).0, [70]);
        assert_eq!(
            layer.pixels[&Channel::BaseColor].get_pixel(11, 10).0,
            [12, 34, 56, 200]
        );
    }

    #[test]
    fn project_archive_fingerprint_tracks_exact_bytes() {
        let path = path("archive-digest");
        std::fs::write(&path, b"project bytes").unwrap();
        let expected: [u8; 32] = Sha256::digest(b"project bytes").into();
        assert_eq!(file_digest(&path).unwrap(), expected);
        std::fs::write(&path, b"different project bytes").unwrap();
        assert_ne!(file_digest(&path).unwrap(), expected);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn project_archive_fingerprint_rejects_oversized_files_before_reading() {
        let path = path("archive-digest-limit");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_PROJECT_ARCHIVE_BYTES + 1).unwrap();
        let error = match file_digest(&path) {
            Ok(_) => panic!("oversized project fingerprint should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("1-GiB recovery fingerprint limit"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn project_load_rejects_oversized_sparse_archive_before_parsing() {
        let path = path("archive-load-limit");
        let cleanup = Cleanup(vec![path.clone()]);
        let file = File::create(&path).unwrap();
        file.set_len(MAX_PROJECT_ARCHIVE_BYTES + 1).unwrap();
        drop(file);

        let error = match load(&path) {
            Ok(_) => panic!("oversized project archive should be rejected"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("1-GiB load limit"),
            "{error:#}"
        );
        drop(cleanup);
    }

    #[test]
    fn checkpoint_round_trip_preserves_document_and_leaves_primary_unchanged() {
        let primary = path("checkpoint-primary");
        let checkpoint = path("checkpoint-snapshot");
        let cleanup = Cleanup(vec![
            primary.clone(),
            temp_path(&primary),
            checkpoint.clone(),
            temp_path(&checkpoint),
        ]);

        let mut original = Document::new(64);
        original.add_layer().unwrap();
        original.layer_tree.paint_layer_mut(2).unwrap().name = "Checkpoint detail".into();
        original
            .layer_tree
            .paint_layer_mut(2)
            .unwrap()
            .mask
            .put_pixel(7, 9, Luma([123]));
        original
            .layer_tree
            .paint_layer_mut(2)
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 9, Rgba([31, 62, 93, 187]));
        original
            .layer_tree
            .paint_layer_mut(2)
            .unwrap()
            .extensions
            .insert(
                "future_layer_field".into(),
                serde_json::json!({"preserve": [1, 2, 3]}),
            );
        original.extensions.insert(
            "future_project_field".into(),
            serde_json::json!({"label": "checkpoint"}),
        );

        let second_set_id = original.add_texture_set("Checkpoint alternate").unwrap();
        original.texture_sets[0].extensions.insert(
            "future_set_field".into(),
            serde_json::json!({"opaque": true}),
        );
        original.material_bindings = vec![Some(1)];
        original
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(2, 4, Rgba([128, 128, 255, 255]));
        assert_eq!(original.active_set_id, second_set_id);
        original.validate().unwrap();

        let mut primary_doc = Document::new(64);
        set_marker(&mut primary_doc, 17);
        save(&primary_doc, &primary).unwrap();
        let primary_bytes_before = std::fs::read(&primary).unwrap();

        save_checkpoint(&original, &checkpoint).unwrap();
        let checkpoint_loaded = load_checkpoint(&checkpoint).unwrap();

        assert_eq!(std::fs::read(&primary).unwrap(), primary_bytes_before);
        assert_eq!(marker(&load(&primary).unwrap()), [17, 0, 0, 255]);

        assert_eq!(checkpoint_loaded.size, original.size);
        assert_eq!(checkpoint_loaded.active_set_id, original.active_set_id);
        assert_eq!(checkpoint_loaded.active_set_name, original.active_set_name);
        assert_eq!(
            checkpoint_loaded.material_bindings,
            original.material_bindings
        );
        assert_eq!(checkpoint_loaded.texture_sets.len(), 1);
        assert_eq!(checkpoint_loaded.texture_sets[0].id, 1);
        assert_eq!(checkpoint_loaded.texture_sets[0].name, "Default");
        assert_eq!(
            checkpoint_loaded.layer_tree.ordered_paint_leaf_ids().len(),
            1
        );
        assert_eq!(
            checkpoint_loaded.selected_layer().unwrap().pixels[&Channel::Normal]
                .get_pixel(2, 4)
                .0,
            [128, 128, 255, 255]
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0]
                .layer_tree
                .paint_layer(2)
                .unwrap()
                .mask
                .get_pixel(7, 9)
                .0,
            [123]
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0]
                .layer_tree
                .paint_layer(2)
                .unwrap()
                .pixels[&Channel::BaseColor]
                .get_pixel(7, 9)
                .0,
            [31, 62, 93, 187]
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0]
                .layer_tree
                .paint_layer(2)
                .unwrap()
                .extensions["future_layer_field"],
            serde_json::json!({"preserve": [1, 2, 3]})
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0].extensions["future_set_field"],
            serde_json::json!({"opaque": true})
        );
        assert_eq!(
            checkpoint_loaded.extensions["future_project_field"],
            serde_json::json!({"label": "checkpoint"})
        );
        drop(cleanup);
    }

    #[test]
    fn invalid_manifest_metadata_is_rejected_before_layer_image_decode() {
        let doc = Document::new(64);
        let path = path("metadata-preflight");
        save(&doc, &path).unwrap();
        rewrite_manifest_and_entry(
            &path,
            |manifest| manifest["layer_tree"]["roots"][0]["opacity"] = serde_json::json!(2.0),
            ("layers/1/mask.png", b"invalid image payload"),
        );

        let error = match load(&path) {
            Ok(_) => panic!("invalid metadata should be rejected"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("Invalid layer tree node properties"),
            "{error:#}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupted_layer_image_entry_is_rejected_without_panicking() {
        let path = path("corrupted-image-entry");
        let cleanup = Cleanup(vec![path.clone()]);
        save(&Document::new(64), &path).unwrap();
        // A tiny, deterministic non-image payload exercises the full ZIP and
        // manifest path without creating a large decompression fixture.
        rewrite_manifest_and_entry(
            &path,
            |_| {},
            ("layers/1/mask.png", b"\x89PNG\r\n\x1a\ntruncated"),
        );

        let result = std::panic::catch_unwind(|| load(&path));
        let error =
            match result.unwrap_or_else(|_| panic!("loading a corrupted image entry panicked")) {
                Ok(_) => panic!("corrupted image entry should be rejected"),
                Err(error) => error,
            };
        assert!(
            format!("{error:#}").contains("layers/1/mask.png"),
            "{error:#}"
        );
        drop(cleanup);
    }

    #[test]
    fn malformed_manifest_entry_is_rejected_without_panicking() {
        let path = path("malformed-manifest-entry");
        let cleanup = Cleanup(vec![path.clone()]);
        save(&Document::new(64), &path).unwrap();
        rewrite_manifest_and_entry(&path, |_| {}, ("manifest.json", b"{\"version\": ["));

        let result = std::panic::catch_unwind(|| load(&path));
        let error = match result.unwrap_or_else(|_| panic!("loading a malformed manifest panicked"))
        {
            Ok(_) => panic!("malformed manifest should be rejected"),
            Err(error) => error,
        };
        assert!(!format!("{error:#}").is_empty());
        drop(cleanup);
    }

    #[test]
    fn rejects_compressed_layer_entry_over_read_cap_before_image_decode() {
        let size = 64u64;
        let entry_cap = size * size * 4 + 4 * 1024 * 1024;
        let payload = vec![0u8; entry_cap as usize + 1];
        let doc = Document::new(size as u32);
        let path = path("compressed-entry-cap");
        save(&doc, &path).unwrap();
        rewrite_manifest_and_entry(&path, |_| {}, ("layers/1/mask.png", &payload));

        let archive_bytes = std::fs::metadata(&path).unwrap().len();
        assert!(
            archive_bytes < 64 * 1024,
            "compressed archive was {archive_bytes} bytes"
        );

        let error = match load(&path) {
            Ok(_) => panic!("oversized expanded entry should be rejected"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("File 'layers/1/mask.png' exceeds safety limit"),
            "{error:#}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_duplicate_zip_entry_names() {
        let doc = Document::new(64);
        let path = path("duplicate-entry");
        save(&doc, &path).unwrap();
        duplicate_central_directory_entry(&path, "manifest.json");

        let error = match load(&path) {
            Ok(_) => panic!("duplicate ZIP entries should be rejected"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("Duplicate ZIP entry")
                || message.contains("Duplicate filename")
                || message.contains("directory changed while it was being opened"),
            "unexpected duplicate-entry error: {message}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unexpected_zip_payload_entries() {
        let doc = Document::new(64);
        let path = path("unexpected-entry");
        save(&doc, &path).unwrap();
        add_archive_entry(&path, "future/ignored.bin", b"unknown payload");

        let error = match load(&path) {
            Ok(_) => panic!("unknown ZIP payloads should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("Unknown archive entry"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_manifest_texture_budget_before_reading_missing_images() {
        let doc = Document::new(64);
        let path = path("manifest-budget");
        save(&doc, &path).unwrap();
        rewrite_manifest(&path, |manifest| {
            manifest["size"] = serde_json::json!(MAX_SIZE);
            manifest["layer_tree"]["roots"][0]["channels"] = serde_json::json!(Channel::ALL);
        });

        let error = match load(&path) {
            Ok(_) => panic!("over-budget manifest should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("512-MiB decoded texture budget"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovery_prefers_valid_primary_over_valid_temp_archive() {
        let primary = path("recovery-primary-wins");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut primary_doc = Document::new(64);
        set_marker(&mut primary_doc, 21);
        let mut temp_doc = Document::new(64);
        set_marker(&mut temp_doc, 88);
        save(&primary_doc, &primary).unwrap();
        let source = path("recovery-primary-wins-source");
        let _source_cleanup = Cleanup(vec![source.clone(), temp_path(&source)]);
        save(&temp_doc, &source).unwrap();
        std::fs::copy(source, &tmp).unwrap();

        match load_with_recovery(&primary).unwrap() {
            RecoveryOutcome::Primary(doc) => assert_eq!(marker(&doc), [21, 0, 0, 255]),
            RecoveryOutcome::RecoveredTemporary(_) => panic!("valid primary must take precedence"),
        }
        assert_eq!(marker(&load(&primary).unwrap()), [21, 0, 0, 255]);
    }

    #[test]
    fn recovery_loads_fully_valid_temp_when_primary_is_missing() {
        let primary = path("recovery-temp");
        let tmp = temp_path(&primary);
        let source = path("recovery-temp-source");
        let _cleanup = Cleanup(vec![
            primary.clone(),
            tmp.clone(),
            source.clone(),
            temp_path(&source),
        ]);
        let mut doc = Document::new(64);
        set_marker(&mut doc, 73);
        save(&doc, &source).unwrap();
        std::fs::copy(&source, &tmp).unwrap();

        match load_with_recovery(&primary).unwrap() {
            RecoveryOutcome::RecoveredTemporary(recovered) => {
                assert_eq!(marker(&recovered), [73, 0, 0, 255]);
            }
            RecoveryOutcome::Primary(_) => panic!("missing primary must report temp recovery"),
        }
        assert!(
            !primary.exists(),
            "recovery must not promote/overwrite files"
        );
        assert!(
            tmp.exists(),
            "recovery must leave the source temp archive intact"
        );
    }

    #[test]
    fn recovery_rejects_partial_or_corrupt_temp_archive() {
        let primary = path("recovery-corrupt");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        std::fs::write(&primary, b"not a project").unwrap();
        std::fs::write(&tmp, b"PK\x03\x04partial zip archive").unwrap();

        let error = match load_with_recovery(&primary) {
            Ok(_) => panic!("corrupt primary and temp must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Could not load primary project"));
        assert!(error.to_string().contains("recover temporary archive"));
    }

    #[test]
    fn failed_replace_preserves_primary_and_retains_valid_recovery_archive() {
        let primary = path("recovery-injected-replace-failure");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut previous = Document::new(64);
        set_marker(&mut previous, 12);
        save(&previous, &primary).unwrap();
        let mut pending = Document::new(64);
        set_marker(&mut pending, 199);

        let error = save_with_replace(&pending, &primary, |_, _| {
            Err(std::io::Error::other("injected replace failure"))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected replace failure"));
        assert_eq!(marker(&load(&primary).unwrap()), [12, 0, 0, 255]);
        assert!(
            tmp.exists(),
            "completed temp archive should remain recoverable"
        );
        match load_with_recovery(&primary).unwrap() {
            RecoveryOutcome::Primary(doc) => assert_eq!(marker(&doc), [12, 0, 0, 255]),
            RecoveryOutcome::RecoveredTemporary(_) => panic!("valid primary must take precedence"),
        }

        std::fs::remove_file(&primary).unwrap();
        match load_with_recovery(&primary).unwrap() {
            RecoveryOutcome::RecoveredTemporary(doc) => {
                assert_eq!(marker(&doc), [199, 0, 0, 255]);
            }
            RecoveryOutcome::Primary(_) => panic!("expected retained temp recovery"),
        }
    }

    #[test]
    fn create_write_and_temp_sync_failures_preserve_primary_bytes() {
        let primary = path("durable-save-pre-replace-failures");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut previous = Document::new(64);
        set_marker(&mut previous, 17);
        save(&previous, &primary).unwrap();
        let old_bytes = std::fs::read(&primary).unwrap();
        let mut pending = Document::new(64);
        set_marker(&mut pending, 201);

        let create_error = save_with_ops::<File, _, _, _, _>(
            &pending,
            &primary,
            |_| Err(std::io::Error::other("injected temp create failure")),
            File::sync_all,
            replace_project_file,
            sync_parent_directory,
        )
        .unwrap_err();
        assert!(format!("{create_error:#}").contains("injected temp create failure"));
        assert_eq!(std::fs::read(&primary).unwrap(), old_bytes);
        assert!(!tmp.exists());

        let write_error = save_with_ops(
            &pending,
            &primary,
            |temp| {
                Ok(FailingWriter {
                    file: File::create(temp)?,
                    bytes_until_failure: 128,
                })
            },
            |_| Ok(()),
            replace_project_file,
            sync_parent_directory,
        )
        .unwrap_err();
        assert!(format!("{write_error:#}").contains("injected archive write failure"));
        assert_eq!(std::fs::read(&primary).unwrap(), old_bytes);
        assert!(!tmp.exists(), "partial archive must be removed");

        let sync_error = save_with_ops(
            &pending,
            &primary,
            |temp| File::create(temp),
            |_| Err(std::io::Error::other("injected temp sync failure")),
            replace_project_file,
            sync_parent_directory,
        )
        .unwrap_err();
        assert!(format!("{sync_error:#}").contains("injected temp sync failure"));
        assert_eq!(std::fs::read(&primary).unwrap(), old_bytes);
        assert!(
            !tmp.exists(),
            "unsynced archive must not be offered as recovery"
        );
        assert_eq!(marker(&load(&primary).unwrap()), [17, 0, 0, 255]);
    }

    #[test]
    fn archive_finish_failure_preserves_primary_and_removes_incomplete_temp() {
        let primary = path("durable-save-finish-failure");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut previous = Document::new(64);
        set_marker(&mut previous, 19);
        save(&previous, &primary).unwrap();
        let old_bytes = std::fs::read(&primary).unwrap();
        let mut pending = Document::new(64);
        set_marker(&mut pending, 203);
        let written = Rc::new(Cell::new(0usize));

        // Measure a successful archive's complete sequence of writes. ZIP
        // finalization emits the final central-directory footer, so failing on
        // its last byte forces `ZipWriter::finish` to fail after all entries.
        let measure = Rc::clone(&written);
        let measurement_error = save_with_ops(
            &pending,
            &primary,
            move |temp| {
                Ok(CountingWriter {
                    file: File::create(temp)?,
                    bytes_written: measure,
                })
            },
            |writer| writer.file.sync_all(),
            |_, _| Err(std::io::Error::other("stop after measuring archive")),
            sync_parent_directory,
        )
        .unwrap_err();
        assert!(format!("{measurement_error:#}").contains("stop after measuring archive"));
        assert!(written.get() > 1);
        assert!(
            load(&tmp).is_ok(),
            "measurement should produce a complete ZIP"
        );

        let bytes_before_finish_error = written.get();
        let finish_error = save_with_ops(
            &pending,
            &primary,
            |temp| {
                Ok(FailingWriter {
                    file: File::create(temp)?,
                    bytes_until_failure: bytes_before_finish_error - 1,
                })
            },
            |_| Ok(()),
            replace_project_file,
            sync_parent_directory,
        )
        .unwrap_err();
        assert!(format!("{finish_error:#}").contains("injected archive write failure"));
        assert_eq!(std::fs::read(&primary).unwrap(), old_bytes);
        assert!(
            !tmp.exists(),
            "incomplete ZIP must not remain as recovery data"
        );
    }

    #[test]
    fn completed_temp_is_synced_before_replace_failure_and_remains_recoverable() {
        let primary = path("durable-save-replace-failure");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut previous = Document::new(64);
        set_marker(&mut previous, 29);
        save(&previous, &primary).unwrap();
        let old_bytes = std::fs::read(&primary).unwrap();
        let mut pending = Document::new(64);
        set_marker(&mut pending, 211);
        let synced = std::cell::Cell::new(false);

        let error = save_with_ops(
            &pending,
            &primary,
            |temp| File::create(temp),
            |_| {
                synced.set(true);
                Ok(())
            },
            |_, _| {
                assert!(synced.get(), "replacement must follow temp sync");
                Err(std::io::Error::other("injected replacement failure"))
            },
            sync_parent_directory,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected replacement failure"));
        assert!(synced.get());
        assert_eq!(std::fs::read(&primary).unwrap(), old_bytes);
        assert_eq!(marker(&load(&tmp).unwrap()), [211, 0, 0, 255]);
        match load_with_recovery(&primary).unwrap() {
            RecoveryOutcome::Primary(doc) => assert_eq!(marker(&doc), [29, 0, 0, 255]),
            RecoveryOutcome::RecoveredTemporary(_) => panic!("valid primary takes precedence"),
        }
    }

    #[test]
    fn directory_sync_failure_reports_that_replacement_already_happened() {
        let primary = path("durable-save-dir-sync-failure");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut previous = Document::new(64);
        set_marker(&mut previous, 31);
        save(&previous, &primary).unwrap();
        let mut pending = Document::new(64);
        set_marker(&mut pending, 217);

        let error = save_with_ops(
            &pending,
            &primary,
            |temp| File::create(temp),
            File::sync_all,
            replace_project_file,
            |_| Err(std::io::Error::other("injected directory sync failure")),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(error.downcast_ref::<PostReplaceSyncError>().is_some());
        assert!(message.contains("replaced successfully"), "{message}");
        assert!(
            message.contains("injected directory sync failure"),
            "{message}"
        );
        assert_eq!(marker(&load(&primary).unwrap()), [217, 0, 0, 255]);
        assert!(!tmp.exists());
    }

    #[test]
    fn successful_save_replaces_existing_primary_and_reopens_exact_content() {
        let primary = path("durable-save-success");
        let tmp = temp_path(&primary);
        let _cleanup = Cleanup(vec![primary.clone(), tmp.clone()]);
        let mut previous = Document::new(64);
        set_marker(&mut previous, 33);
        save(&previous, &primary).unwrap();
        let mut pending = Document::new(64);
        set_marker(&mut pending, 223);
        let expected_project_id = pending.project_id;

        save(&pending, &primary).unwrap();

        let reopened = load(&primary).unwrap();
        assert_eq!(reopened.project_id, expected_project_id);
        assert_eq!(marker(&reopened), [223, 0, 0, 255]);
        assert!(!tmp.exists());
    }

    #[test]
    fn multiple_texture_sets_roundtrip_with_distinct_layers() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(0, 0, Rgba([1, 2, 3, 255]));
        let id = doc.add_texture_set("Armor / Leather").unwrap();
        doc.try_resize(128).unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .mask
            .put_pixel(1, 1, Luma([64]));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Metallic, 128)
            .put_pixel(0, 0, Rgba([9, 8, 7, 255]));
        let path = path("sets");
        save(&doc, &path).unwrap();
        let mut restored = load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(restored.texture_set_count(), 2);
        assert_eq!(restored.active_set_id, id);
        assert_eq!(restored.active_set_name, "Armor / Leather");
        assert_eq!(restored.size, 128);
        assert_eq!(
            restored.selected_layer().unwrap().mask.get_pixel(1, 1).0,
            [64]
        );
        restored.activate_texture_set(1).unwrap();
        assert_eq!(restored.size, 64);
        assert_eq!(
            restored.composite(Channel::BaseColor).get_pixel(0, 0).0,
            [1, 2, 3, 255]
        );
        restored.validate().unwrap();
    }

    #[test]
    fn two_material_paint_save_reload_and_export_workflow() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/TwoMaterials.obj");
        let project_path = path("two-material-workflow");
        let export_dir = project_path.with_extension("exports");
        let _cleanup = Cleanup(vec![project_path.clone(), export_dir.clone()]);

        let mut doc = Document::new(64);
        let mesh = Mesh::from_file(&fixture).unwrap();
        doc.import_mesh(mesh).unwrap();
        assert_eq!(doc.mesh.material_name(0), "Leather");
        assert_eq!(doc.mesh.material_name(1), "Metal");

        // The first set paints Leather and the second set paints Metal.
        let leather_set = doc.active_set_id;
        let metal_set = doc.add_texture_set("Metal").unwrap();
        doc.bind_material(0, Some(leather_set)).unwrap();
        doc.bind_material(1, Some(metal_set)).unwrap();

        let materials = crate::renderer::MaterialLibrary::from_document(&doc);
        let mut viewport = crate::renderer::Viewport::new(320, 320);
        viewport.render_bound(&doc.mesh, &materials, &doc.material_bindings, None);
        let pick_hit = |slot| {
            (0..viewport.height).find_map(|y| {
                (0..viewport.width).find_map(|x| {
                    (viewport.pick_material_slot(x, y) == Some(slot))
                        .then(|| (x, y, viewport.pick(x, y).expect("hit has UV coordinates")))
                })
            })
        };
        let (leather_x, leather_y, leather_uv) =
            pick_hit(0).expect("rendered Leather should be pickable");
        let (_, _, metal_uv) = pick_hit(1).expect("rendered Metal should be pickable");
        let before_paint = viewport.rgba[(leather_y * viewport.width + leather_x) * 4
            ..(leather_y * viewport.width + leather_x) * 4 + 4]
            .to_vec();
        let leather_coverage =
            UvCoverage::generate_for_set(&doc.mesh, doc.size, &doc.material_bindings, leather_set)
                .unwrap();
        let metal_coverage =
            UvCoverage::generate_for_set(&doc.mesh, doc.size, &doc.material_bindings, metal_set)
                .unwrap();
        assert!(leather_coverage.contains_uv(leather_uv));
        assert!(!leather_coverage.contains_uv(metal_uv));
        assert!(metal_coverage.contains_uv(metal_uv));
        assert!(!metal_coverage.contains_uv(leather_uv));
        let texel = |uv: [f32; 2]| {
            (
                (uv[0] * (doc.size - 1) as f32).round() as u32,
                (uv[1] * (doc.size - 1) as f32).round() as u32,
            )
        };
        let leather_texel = texel(leather_uv);
        let metal_texel = texel(metal_uv);

        let mut brush = Brush {
            radius: 6,
            opacity: 1.0,
            hardness: 1.0,
            color: [220, 24, 40, 255],
            ..Brush::default()
        };
        doc.activate_texture_set(leather_set).unwrap();
        brush.stamp_masked(
            &mut doc,
            Channel::BaseColor,
            leather_uv,
            Some(&leather_coverage),
        );

        doc.activate_texture_set(metal_set).unwrap();
        brush.grayscale = 190;
        brush.stamp_masked(&mut doc, Channel::Metallic, metal_uv, Some(&metal_coverage));

        let painted_materials = crate::renderer::MaterialLibrary::from_document(&doc);
        viewport.render_bound(&doc.mesh, &painted_materials, &doc.material_bindings, None);
        let screen_offset = (leather_y * viewport.width + leather_x) * 4;
        assert_ne!(
            &viewport.rgba[screen_offset..screen_offset + 4],
            before_paint
        );

        save(&doc, &project_path).unwrap();
        let restored = load(&project_path).unwrap();
        assert_eq!(
            restored.material_bindings,
            vec![Some(leather_set), Some(metal_set)]
        );
        let leather_base = restored
            .composite_for_set(leather_set, Channel::BaseColor)
            .unwrap();
        let leather_metal = restored
            .composite_for_set(leather_set, Channel::Metallic)
            .unwrap();
        let metal_base = restored
            .composite_for_set(metal_set, Channel::BaseColor)
            .unwrap();
        let metal_metal = restored
            .composite_for_set(metal_set, Channel::Metallic)
            .unwrap();
        assert_eq!(
            leather_base.get_pixel(leather_texel.0, leather_texel.1).0,
            [220, 24, 40, 255]
        );
        assert_eq!(
            leather_metal.get_pixel(leather_texel.0, leather_texel.1).0,
            Channel::Metallic.background()
        );
        assert_eq!(
            metal_base.get_pixel(metal_texel.0, metal_texel.1).0,
            Channel::BaseColor.background()
        );
        assert_eq!(
            metal_metal.get_pixel(metal_texel.0, metal_texel.1).0,
            [190, 190, 190, 255]
        );

        export::export_all_texture_sets(&restored, &export_dir).unwrap();
        let exported_leather = image::open(export_dir.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        let exported_metal = image::open(export_dir.join("002_Metal/Kalium_Metallic.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(
            exported_leather
                .get_pixel(leather_texel.0, leather_texel.1)
                .0,
            [220, 24, 40, 255]
        );
        assert_eq!(
            exported_metal.get_pixel(metal_texel.0, metal_texel.1).0,
            [190, 190, 190, 255]
        );
    }

    #[test]
    #[ignore = "manual fixture generator for Blender multi-Texture-Set acceptance"]
    fn write_blender_two_set_fixture() {
        let output = std::env::var_os("KALIUM_BLENDER_FIXTURE_OUTPUT")
            .expect("set KALIUM_BLENDER_FIXTURE_OUTPUT to the destination .kalium path");
        let output = PathBuf::from(output);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let mesh_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/TwoMaterials.obj");
        let mut doc = Document::new(64);
        doc.import_mesh(Mesh::from_file(&mesh_path).unwrap())
            .unwrap();
        assert_eq!(doc.mesh.material_name(0), "Leather");
        assert_eq!(doc.mesh.material_name(1), "Metal");

        let leather_set = doc.active_set_id;
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::BaseColor,
            image::RgbaImage::from_pixel(64, 64, Rgba([220, 24, 40, 255])),
        );
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Metallic,
            image::RgbaImage::from_pixel(64, 64, Rgba([16, 16, 16, 255])),
        );
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Roughness,
            image::RgbaImage::from_pixel(64, 64, Rgba([224, 224, 224, 255])),
        );
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Normal,
            image::RgbaImage::from_pixel(64, 64, Rgba([128, 128, 255, 255])),
        );
        let metal_set = doc.add_texture_set("Metal").unwrap();
        doc.bind_material(0, Some(leather_set)).unwrap();
        doc.bind_material(1, Some(metal_set)).unwrap();
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::BaseColor,
            image::RgbaImage::from_pixel(64, 64, Rgba([28, 90, 220, 255])),
        );
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Metallic,
            image::RgbaImage::from_pixel(64, 64, Rgba([224, 224, 224, 255])),
        );
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Roughness,
            image::RgbaImage::from_pixel(64, 64, Rgba([32, 32, 32, 255])),
        );
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Normal,
            image::RgbaImage::from_pixel(64, 64, Rgba([180, 128, 250, 255])),
        );
        doc.activate_texture_set(leather_set).unwrap();
        save(&doc, &output).unwrap();

        let reopened = load(&output).unwrap();
        assert_eq!(
            reopened.material_bindings,
            vec![Some(leather_set), Some(metal_set)]
        );
        assert_eq!(reopened.texture_set_count(), 2);
        assert_eq!(
            reopened
                .composite_for_set(leather_set, Channel::BaseColor)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [220, 24, 40, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(metal_set, Channel::BaseColor)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [28, 90, 220, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(leather_set, Channel::Roughness)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [224, 224, 224, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(metal_set, Channel::Roughness)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [32, 32, 32, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(leather_set, Channel::Metallic)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [16, 16, 16, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(metal_set, Channel::Metallic)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [224, 224, 224, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(leather_set, Channel::Normal)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [128, 128, 255, 255]
        );
        assert_eq!(
            reopened
                .composite_for_set(metal_set, Channel::Normal)
                .unwrap()
                .get_pixel(0, 0)
                .0,
            [178, 128, 245, 255]
        );
        assert_ne!(leather_set, metal_set);
    }

    #[test]
    fn transformed_gltf_pick_paint_save_reload_and_export_workflow() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-transformed-gltf-workflow-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = dir.join("transformed.gltf");
        let project_path = dir.join("painted.kalium");
        let export_dir = dir.join("export");
        let _cleanup = Cleanup(vec![dir]);

        // Tiny embedded triangle: three positions, three UVs, and three u16 indices.
        // The node transform follows the same TRS convention checked by mesh.rs.
        let mut buffer = Vec::new();
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0.1f32, 0.1, 0.9, 0.1, 0.1, 0.9] {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0u16, 1, 2] {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        let fixture_json = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{
                "mesh": 0,
                "translation": [2, 3, 0],
                "rotation": [0, 0, 0.70710677, 0.70710677],
                "scale": [2, 1, 1]
            }],
            "meshes": [{"primitives": [{
                "attributes": {"POSITION": 0, "TEXCOORD_0": 1},
                "indices": 2,
                "material": 0
            }]}],
            "materials": [{"name": "Transformed"}],
            "buffers": [{
                "byteLength": buffer.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&buffer))
            }],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36},
                {"buffer": 0, "byteOffset": 36, "byteLength": 24},
                {"buffer": 0, "byteOffset": 60, "byteLength": 6}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]},
                {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC2"},
                {"bufferView": 2, "componentType": 5123, "count": 3, "type": "SCALAR"}
            ]
        });
        std::fs::write(&fixture, serde_json::to_vec(&fixture_json).unwrap()).unwrap();

        let mut doc = Document::new(64);
        doc.import_mesh(Mesh::from_file(&fixture).unwrap()).unwrap();
        assert_eq!(doc.mesh.material_name(0), "Transformed");
        let expected = [[0.5, -1.0, 0.0], [0.5, 1.0, 0.0], [-0.5, -1.0, 0.0]];
        for (vertex, expected) in doc.mesh.vertices.iter().zip(expected) {
            for (actual, expected) in vertex.position.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-5, "{actual} != {expected}");
            }
        }

        let set_id = doc.active_set_id;
        let materials = crate::renderer::MaterialLibrary::from_document(&doc);
        let mut viewport = crate::renderer::Viewport::new(320, 320);
        viewport.render_bound(&doc.mesh, &materials, &doc.material_bindings, None);
        let (x, y, uv) = (0..viewport.height)
            .find_map(|y| {
                (0..viewport.width).find_map(|x| {
                    (viewport.pick_material_slot(x, y) == Some(0))
                        .then(|| (x, y, viewport.pick(x, y).expect("material hit has UV")))
                })
            })
            .expect("transformed glTF triangle should render and be pickable");
        let coverage =
            UvCoverage::generate_for_set(&doc.mesh, doc.size, &doc.material_bindings, set_id)
                .unwrap();
        assert!(
            coverage.contains_uv(uv),
            "picked UV must map to imported triangle"
        );
        let texel = (
            (uv[0] * (doc.size - 1) as f32).round() as u32,
            (uv[1] * (doc.size - 1) as f32).round() as u32,
        );
        let brush = Brush {
            radius: 6,
            opacity: 1.0,
            hardness: 1.0,
            color: [18, 190, 72, 255],
            ..Brush::default()
        };
        brush.stamp_masked(&mut doc, Channel::BaseColor, uv, Some(&coverage));

        save(&doc, &project_path).unwrap();
        let restored = load(&project_path).unwrap();
        assert_eq!(restored.mesh.material_name(0), "Transformed");
        assert_eq!(
            restored.mesh.vertices[0].position,
            doc.mesh.vertices[0].position
        );
        let painted = restored
            .composite_for_set(set_id, Channel::BaseColor)
            .unwrap();
        assert_eq!(painted.get_pixel(texel.0, texel.1).0, [18, 190, 72, 255]);
        export::export_all_texture_sets(&restored, &export_dir).unwrap();
        let exported = image::open(export_dir.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(exported.get_pixel(texel.0, texel.1).0, [18, 190, 72, 255]);
        assert!(viewport.pick(x, y).is_some());
    }

    #[test]
    fn mesh_material_bindings_roundtrip_current_version() {
        let mut doc = Document::new(64);
        let second = doc.add_texture_set("Leather").unwrap();
        doc.mesh.materials.push("Glove".into());
        doc.mesh.triangle_materials[0] = 1;
        doc.mesh.vertices[0].tangent = Some([1.0, 0.0, 0.0, 1.0]);
        let extra_uvs = doc
            .mesh
            .vertices
            .iter()
            .enumerate()
            .map(|(index, _)| Some([index as f32 / 8.0, 0.375]))
            .collect();
        doc.mesh.extra_uv_sets.insert(1, extra_uvs);
        doc.material_bindings = vec![Some(second), None];
        let path = path("material-bindings");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.material_bindings, vec![Some(second), None]);
        assert_eq!(loaded.mesh.triangle_material(0), 1);
        assert_eq!(loaded.mesh.material_name(1), "Glove");
        assert_eq!(loaded.mesh.vertices[0].tangent, Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(loaded.mesh.extra_uv_sets, doc.mesh.extra_uv_sets);
    }

    #[test]
    fn v4_project_loads_and_saves_as_v9_with_stable_identity() {
        let mut doc = Document::new(64);
        doc.mesh.materials.push("Second material".into());
        doc.material_bindings = vec![None, None];
        let path = path("v4-to-v8");
        save(&doc, &path).unwrap();
        rewrite_as_legacy_version(&path, 4);
        rewrite_manifest(&path, |manifest| {
            manifest.as_object_mut().unwrap().remove("project_id");
        });

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.material_bindings, vec![None, None]);
        let migrated_id = loaded.project_id;
        assert!(!migrated_id.is_nil());
        save(&loaded, &path).unwrap();
        let manifest = saved_manifest(&path);
        assert_eq!(manifest["version"], 9);
        assert_eq!(manifest["project_id"], migrated_id.to_string());
        assert_eq!(load(&path).unwrap().project_id, migrated_id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn v1_through_v5_identity_is_stable_across_reloads_and_copies() {
        for version in 1..=5 {
            let original = path(&format!("legacy-identity-v{version}"));
            let copied = path(&format!("legacy-identity-v{version}-copy"));
            let _cleanup = Cleanup(vec![original.clone(), copied.clone()]);
            save(&Document::new(64), &original).unwrap();
            rewrite_as_legacy_version(&original, version);
            let original_bytes = std::fs::read(&original).unwrap();
            std::fs::copy(&original, &copied).unwrap();

            let first = load(&original).unwrap();
            let second = load(&original).unwrap();
            let copy = load(&copied).unwrap();
            assert!(!first.project_id.is_nil(), "v{version}");
            assert_eq!(first.project_id.as_bytes()[6] >> 4, 8, "v{version}");
            assert_eq!(first.project_id.as_bytes()[8] & 0xc0, 0x80, "v{version}");
            assert_eq!(first.project_id, second.project_id, "v{version}");
            assert_eq!(first.project_id, copy.project_id, "v{version}");
            assert_eq!(std::fs::read(&original).unwrap(), original_bytes);

            let migrated_id = first.project_id;
            save(&first, &original).unwrap();
            assert_eq!(saved_manifest(&original)["version"], 9);
            assert_eq!(
                saved_manifest(&original)["project_id"],
                migrated_id.to_string()
            );
            assert_eq!(load(&original).unwrap().project_id, migrated_id);
        }
    }

    #[test]
    fn changing_legacy_archive_bytes_changes_derived_identity() {
        let original = path("legacy-identity-changed-original");
        let changed = path("legacy-identity-changed-copy");
        let _cleanup = Cleanup(vec![original.clone(), changed.clone()]);
        save(&Document::new(64), &original).unwrap();
        rewrite_as_legacy_version(&original, 5);
        std::fs::copy(&original, &changed).unwrap();
        rewrite_manifest(&changed, |manifest| {
            manifest["layers"][0]["name"] = serde_json::json!("Changed archive");
        });

        let original_id = load(&original).unwrap().project_id;
        let changed_id = load(&changed).unwrap().project_id;
        assert_ne!(original_id, changed_id);
    }

    #[test]
    fn v6_through_v9_keep_their_explicit_project_identity() {
        let source = path("explicit-project-identity-source");
        let _cleanup = Cleanup(vec![source.clone()]);
        let document = Document::new(64);
        let expected = document.project_id;
        save(&document, &source).unwrap();

        for version in 6..=9 {
            let target = path(&format!("explicit-project-identity-v{version}"));
            let _target_cleanup = Cleanup(vec![target.clone()]);
            std::fs::copy(&source, &target).unwrap();
            if version < 9 {
                rewrite_as_legacy_version(&target, version);
            }
            assert_eq!(load(&target).unwrap().project_id, expected, "v{version}");
        }
    }

    #[test]
    fn integrated_blend_modes_roundtrip_without_changing_composite() {
        for blend in BlendMode::ALL {
            let mut doc = Document::new(64);
            for channel in Channel::ALL {
                for pixel in doc
                    .layer_tree
                    .paint_layer_mut(1)
                    .unwrap()
                    .pixels_mut(channel, 64)
                    .pixels_mut()
                {
                    *pixel = Rgba([80, 130, 210, 255]);
                }
            }
            doc.add_layer().unwrap();
            let layer = doc.selected_paint_layer_mut().unwrap();
            layer.blend = blend;
            layer.opacity = 0.625;
            for channel in Channel::ALL {
                for pixel in layer.pixels_mut(channel, 64).pixels_mut() {
                    *pixel = Rgba([190, 95, 150, 192]);
                }
            }
            let before = Channel::ALL.map(|channel| doc.composite(channel));
            let target = path("subcraft-blend-roundtrip");
            let _cleanup = Cleanup(vec![target.clone()]);
            save(&doc, &target).unwrap();
            let loaded = load(&target).unwrap();
            assert_eq!(loaded.selected_paint_layer().unwrap().blend, blend);
            for (channel, expected) in Channel::ALL.into_iter().zip(before) {
                assert_eq!(loaded.composite(channel), expected, "{blend:?} {channel:?}");
            }
        }
    }

    #[test]
    fn detail_normal_blend_roundtrips_in_v9_manifest() {
        let mut doc = Document::new(64);
        doc.layer_tree.paint_layer_mut(1).unwrap().blend = BlendMode::Detail;
        let path = path("detail-normal-v8");
        save(&doc, &path).unwrap();
        let manifest = saved_manifest(&path);
        assert_eq!(manifest["version"], 9);
        assert_eq!(manifest["layer_tree"]["roots"][0]["blend"], "Detail");
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.layer_tree.paint_layer(1).unwrap().blend,
            BlendMode::Detail
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn per_texture_set_ao_settings_roundtrip_in_v9_manifest() {
        let mut doc = Document::new(64);
        let first_id = doc.active_set_id;
        let first_settings = crate::bake::AmbientOcclusionBakeSettings {
            sample_count: 31,
            max_ray_distance: 2.5,
            seed: 81,
        };
        doc.active_set_extensions.insert(
            crate::bake::AO_SETTINGS_EXTENSION_KEY.into(),
            serde_json::to_value(first_settings).unwrap(),
        );
        let second_id = doc.add_texture_set("Second").unwrap();
        let second_settings = crate::bake::AmbientOcclusionBakeSettings {
            sample_count: 12,
            max_ray_distance: 0.75,
            seed: 902,
        };
        doc.active_set_extensions.insert(
            crate::bake::AO_SETTINGS_EXTENSION_KEY.into(),
            serde_json::to_value(second_settings).unwrap(),
        );

        let path = path("texture-set-ao-settings");
        save(&doc, &path).unwrap();
        let manifest = saved_manifest(&path);
        assert_eq!(manifest["version"], 9);
        let mut loaded = load(&path).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::bake::AmbientOcclusionBakeSettings>(
                loaded.active_set_extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY].clone()
            )
            .unwrap(),
            second_settings
        );
        loaded.activate_texture_set(first_id).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::bake::AmbientOcclusionBakeSettings>(
                loaded.active_set_extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY].clone()
            )
            .unwrap(),
            first_settings
        );
        loaded.activate_texture_set(second_id).unwrap();
        save(&loaded, &path).unwrap();
        assert_eq!(
            load(&path).unwrap().active_set_extensions[crate::bake::AO_SETTINGS_EXTENSION_KEY],
            serde_json::to_value(second_settings).unwrap()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unknown_manifest_fields_roundtrip_at_all_supported_levels() {
        let mut doc = Document::new(64);
        let active_set_id = doc.add_texture_set("Leather").unwrap();
        let path = path("unknown-manifest-fields");
        save(&doc, &path).unwrap();
        let root_extension = serde_json::json!({"future": [1, "root", {"enabled": true}]});
        let set_extension = serde_json::json!({"futureSet": {"version": 7, "tags": ["a", "b"]}});
        let layer_extension =
            serde_json::json!({"futureLayer": {"mode": "weighted", "value": 0.25}});
        let mesh_extension = serde_json::json!({"futureMesh": {"schema": 2}});
        let vertex_extension = serde_json::json!({"futureVertex": [false, 12.5]});
        rewrite_manifest(&path, |manifest| {
            let object = manifest.as_object_mut().unwrap();
            object.insert("futureRoot".into(), root_extension.clone());
            object.get_mut("texture_sets").unwrap()[0]["futureSet"] = set_extension.clone();
            object.get_mut("layer_tree").unwrap()["futureTree"] =
                serde_json::json!({"revision": 4});
            object.get_mut("layer_tree").unwrap()["roots"][0]["futureLayer"] =
                layer_extension.clone();
            object.get_mut("mesh").unwrap()["futureMesh"] = mesh_extension.clone();
            object.get_mut("mesh").unwrap()["vertices"][0]["futureVertex"] =
                vertex_extension.clone();
        });

        let mut loaded = load(&path).unwrap();
        assert_eq!(loaded.extensions.get("futureRoot"), Some(&root_extension));
        assert_eq!(
            loaded.layer_tree.extensions.get("futureTree"),
            Some(&serde_json::json!({"revision": 4}))
        );
        assert_eq!(
            loaded.texture_sets[0].extensions.get("futureSet"),
            Some(&set_extension)
        );
        assert_eq!(
            loaded
                .layer_tree
                .paint_layer(1)
                .unwrap()
                .extensions
                .get("futureLayer"),
            Some(&layer_extension)
        );
        assert_eq!(
            loaded.mesh.extensions.get("futureMesh"),
            Some(&mesh_extension)
        );
        assert_eq!(
            loaded.mesh.vertices[0].extensions.get("futureVertex"),
            Some(&vertex_extension)
        );

        // Root project extensions remain independent of the active-set state.
        loaded.activate_texture_set(1).unwrap();
        loaded.activate_texture_set(active_set_id).unwrap();
        save(&loaded, &path).unwrap();
        let reopened = load(&path).unwrap();
        assert_eq!(reopened.extensions.get("futureRoot"), Some(&root_extension));
        assert_eq!(
            reopened.layer_tree.extensions.get("futureTree"),
            Some(&serde_json::json!({"revision": 4}))
        );
        assert_eq!(
            reopened.texture_sets[0].extensions.get("futureSet"),
            Some(&set_extension)
        );
        assert_eq!(
            reopened
                .layer_tree
                .paint_layer(1)
                .unwrap()
                .extensions
                .get("futureLayer"),
            Some(&layer_extension)
        );
        assert_eq!(
            reopened.mesh.extensions.get("futureMesh"),
            Some(&mesh_extension)
        );
        assert_eq!(
            reopened.mesh.vertices[0].extensions.get("futureVertex"),
            Some(&vertex_extension)
        );

        // The selected set is flattened into the root in v1-v5. Its extension
        // fields must still be emitted there when it becomes active.
        loaded.activate_texture_set(1).unwrap();
        save(&loaded, &path).unwrap();
        assert_eq!(saved_manifest(&path)["futureSet"], set_extension);
        let active_set_reopened = load(&path).unwrap();
        assert_eq!(
            active_set_reopened.extensions.get("futureRoot"),
            Some(&root_extension)
        );
        assert_eq!(
            active_set_reopened.extensions.get("futureSet"),
            Some(&set_extension)
        );
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn legacy_v1_manifest_defaults_to_enabled_channels() {
        let json = r#"{"id":1,"name":"Legacy","visible":true,"opacity":1.0,"blend":"Normal","channels":["BaseColor"]}"#;
        let layer: LayerManifest = serde_json::from_str(json).unwrap();
        assert!(layer.disabled_channels.is_empty());
    }

    #[test]
    fn bundled_v1_demo_remains_loadable() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Kalium-Demo.kalium");
        let doc = load(&path).unwrap();
        assert!(doc.layer_tree.ordered_paint_leaf_ids().iter().all(|id| doc
            .layer_tree
            .paint_layer(*id)
            .unwrap()
            .disabled_channels
            .is_empty()));
        assert_eq!(doc.texture_set_count(), 1);
        assert_eq!(doc.active_set_id, 1);
        assert!(!doc.project_id.is_nil());
        doc.validate().unwrap();
    }

    #[test]
    fn every_legacy_version_migrates_and_resaves_as_v9() {
        let mut source = Document::new(64);
        source.add_layer().unwrap();
        source.layer_tree.select(2).unwrap();
        let paint = source.layer_tree.paint_layer_mut(2).unwrap();
        paint.name = "Migration fixture".into();
        paint.opacity = 0.625;
        paint.blend = BlendMode::Multiply;
        paint.mask.put_pixel(4, 5, Luma([93]));
        paint
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(4, 5, Rgba([23, 54, 87, 201]));
        paint
            .extensions
            .insert("futureLayer".into(), serde_json::json!({"preserve": true}));
        let expected_composite = source.layer_tree.composite(64, Channel::BaseColor);

        for version in 1..=8 {
            let path = path(&format!("legacy-v{version}-migration-roundtrip"));
            save(&source, &path).unwrap();
            rewrite_as_legacy_version(&path, version);

            let migrated = load(&path).unwrap();
            assert_eq!(migrated.layer_tree.ordered_paint_leaf_ids(), vec![1, 2]);
            assert_eq!(migrated.layer_tree.selected_node_id, 2);
            assert_eq!(migrated.layer_tree.next_node_id, 3);
            assert_eq!(
                migrated.layer_tree.composite(64, Channel::BaseColor),
                expected_composite,
                "composite changed while migrating v{version}"
            );
            let layer = migrated.layer_tree.paint_layer(2).unwrap();
            assert_eq!(layer.name, "Migration fixture");
            assert_eq!(layer.opacity, 0.625);
            assert_eq!(layer.blend, BlendMode::Multiply);
            assert_eq!(layer.mask.get_pixel(4, 5).0, [93]);
            assert_eq!(
                layer.pixels[&Channel::BaseColor].get_pixel(4, 5).0,
                [23, 54, 87, 201]
            );
            assert_eq!(
                layer.extensions["futureLayer"],
                serde_json::json!({"preserve": true})
            );

            save(&migrated, &path).unwrap();
            assert_eq!(saved_manifest(&path)["version"], 9);
            let resaved = load(&path).unwrap();
            assert_eq!(
                resaved.layer_tree.composite(64, Channel::BaseColor),
                expected_composite,
                "composite changed after resaving migrated v{version}"
            );
            assert_eq!(resaved.layer_tree.selected_node_id, 2);
            assert_eq!(
                resaved.layer_tree.paint_layer(2).unwrap().extensions["futureLayer"],
                serde_json::json!({"preserve": true})
            );
            resaved.validate().unwrap();
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn v6_rejects_nil_project_identity() {
        let path = path("nil-project-id");
        save(&Document::new(64), &path).unwrap();
        rewrite_as_legacy_version(&path, 6);
        rewrite_manifest(&path, |manifest| {
            manifest["project_id"] = serde_json::json!(uuid::Uuid::nil());
        });
        let error = match load(&path) {
            Ok(_) => panic!("nil project identity should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("Project identity is missing or invalid"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn procedural_graph_roundtrips_for_active_and_inactive_sets_in_v9() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let inactive_ao_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Occlusion,
            image::RgbaImage::from_pixel(64, 64, image::Rgba([128, 128, 128, 255])),
        );
        let inactive_graph = ProceduralGraph::value_noise_times_occlusion(101, 7.5, inactive_ao_id);
        doc.layer_tree.paint_layer_mut(1).unwrap().procedural_graph = Some(inactive_graph.clone());
        let active_id = doc.add_texture_set("Active Graph").unwrap();
        doc.add_layer().unwrap();
        let active_ao_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Occlusion,
            image::RgbaImage::from_pixel(64, 64, image::Rgba([192, 192, 192, 255])),
        );
        let active_graph = ProceduralGraph::value_noise_times_occlusion(202, 3.0, active_ao_id);
        doc.layer_tree.paint_layer_mut(1).unwrap().procedural_graph = Some(active_graph.clone());

        let path = path("procedural-v8-roundtrip");
        save(&doc, &path).unwrap();
        assert_eq!(saved_manifest(&path)["version"], 9);
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.layer_tree.paint_layer(1).unwrap().procedural_graph,
            Some(active_graph)
        );
        assert_eq!(
            loaded
                .texture_sets
                .iter()
                .find(|set| set.id == 1)
                .unwrap()
                .layer_tree
                .paint_layer(1)
                .unwrap()
                .procedural_graph,
            Some(inactive_graph)
        );
        assert_eq!(
            loaded.composite_for_set(active_id, Channel::Roughness),
            doc.composite_for_set(active_id, Channel::Roughness)
        );
        assert_eq!(
            loaded.composite_for_set(1, Channel::Roughness),
            doc.composite_for_set(1, Channel::Roughness)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn v7_rejects_ao_graph_inputs_that_require_v8() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        let ao_id = doc.selected_layer().unwrap().id;
        doc.selected_layer_mut().unwrap().pixels.insert(
            Channel::Occlusion,
            image::RgbaImage::from_pixel(64, 64, Rgba([255; 4])),
        );
        doc.layer_tree.paint_layer_mut(1).unwrap().procedural_graph =
            Some(ProceduralGraph::value_noise_times_occlusion(2, 4.0, ao_id));
        let path = path("ao-graph-v7-rejected");
        save(&doc, &path).unwrap();
        rewrite_as_legacy_version(&path, 7);
        let error = match load(&path) {
            Ok(_) => panic!("v7 project unexpectedly accepts an AO graph input"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("Legacy project unexpectedly contains AO"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn v6_rejects_unrecognized_procedural_layer_data() {
        let mut doc = Document::new(64);
        doc.layer_tree.paint_layer_mut(1).unwrap().procedural_graph =
            Some(ProceduralGraph::value_noise(2, 4.0));
        let path = path("procedural-v6-rejected");
        save(&doc, &path).unwrap();
        rewrite_as_legacy_version(&path, 6);
        let error = match load(&path) {
            Ok(_) => panic!("v6 project unexpectedly accepts graph layer data"),
            Err(error) => error,
        };
        assert!(format!("{error:#}")
            .contains("Legacy project unexpectedly contains procedural graph data"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn v8_migration_preserves_flat_paint_ids_order_selection_and_metadata() {
        let mut doc = Document::new(64);
        doc.add_layer().unwrap();
        doc.add_layer().unwrap();
        doc.layer_tree.select(2).unwrap();
        let layer = doc.layer_tree.paint_layer_mut(2).unwrap();
        layer.name = "Legacy selected".into();
        layer.visible = false;
        layer.opacity = 0.375;
        layer.blend = BlendMode::Multiply;
        layer.disabled_channels.insert(Channel::Roughness);
        layer.mask.put_pixel(2, 3, Luma([91]));
        layer
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(2, 3, Rgba([12, 34, 56, 78]));
        layer.extensions.insert(
            "futureLayerField".into(),
            serde_json::json!({"nested": [1, true, "kept"]}),
        );
        let path = path("v8-flat-migration");
        save(&doc, &path).unwrap();
        rewrite_as_legacy_version(&path, 8);

        let migrated = load(&path).unwrap();
        assert_eq!(migrated.layer_tree.ordered_paint_leaf_ids(), vec![1, 2, 3]);
        assert_eq!(migrated.layer_tree.selected_node_id, 2);
        assert_eq!(migrated.layer_tree.next_node_id, 4);
        let selected = migrated.layer_tree.paint_layer(2).unwrap();
        assert_eq!(selected.name, "Legacy selected");
        assert!(!selected.visible);
        assert_eq!(selected.opacity, 0.375);
        assert_eq!(selected.blend, BlendMode::Multiply);
        assert!(selected.disabled_channels.contains(&Channel::Roughness));
        assert_eq!(selected.mask.get_pixel(2, 3).0, [91]);
        assert_eq!(
            selected.pixels[&Channel::BaseColor].get_pixel(2, 3).0,
            [12, 34, 56, 78]
        );
        assert_eq!(
            selected.extensions["futureLayerField"],
            serde_json::json!({"nested": [1, true, "kept"]})
        );
        assert_eq!(saved_manifest(&path)["version"], 8);
        let migrated_composite = migrated.layer_tree.composite(64, Channel::BaseColor);
        save(&migrated, &path).unwrap();
        assert_eq!(saved_manifest(&path)["version"], 9);
        let resaved = load(&path).unwrap();
        assert_eq!(resaved.layer_tree.ordered_paint_leaf_ids(), vec![1, 2, 3]);
        assert_eq!(resaved.layer_tree.selected_node_id, 2);
        assert_eq!(
            resaved.layer_tree.composite(64, Channel::BaseColor),
            migrated_composite
        );
        assert_eq!(
            resaved.layer_tree.paint_layer(2).unwrap().extensions["futureLayerField"],
            serde_json::json!({"nested": [1, true, "kept"]})
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn v9_roundtrip_preserves_nested_group_fill_and_any_node_selection() {
        let mut doc = Document::new(64);
        let mut group = doc.layer_tree.allocate_group("Folder", 64).unwrap();
        let group_id = group.properties.id;
        group
            .properties
            .extensions
            .insert("futureGroup".into(), serde_json::json!({"retain": true}));
        doc.layer_tree
            .insert(None, 1, LayerNode::Group(group))
            .unwrap();
        let paint = Layer::new(doc.layer_tree.next_node_id, "Nested paint", 64);
        let paint_id = paint.id;
        doc.layer_tree
            .insert(Some(group_id), 0, LayerNode::Paint(paint))
            .unwrap();
        let fill_id = doc.layer_tree.next_node_id;
        let mut fill_properties = LayerNodeProperties::new(fill_id, "Constant", 64).unwrap();
        fill_properties.opacity = 0.625;
        fill_properties.visible = false;
        fill_properties.mask.put_pixel(8, 9, Luma([117]));
        fill_properties
            .extensions
            .insert("futureFill".into(), serde_json::json!("retained"));
        let fill = FillLayer {
            properties: fill_properties,
            values: [
                (Channel::BaseColor, [21, 43, 65, 255]),
                (Channel::Metallic, [128, 128, 128, 255]),
            ]
            .into_iter()
            .collect(),
        };
        doc.layer_tree
            .insert(Some(group_id), 1, LayerNode::Fill(fill))
            .unwrap();
        doc.layer_tree.select(fill_id).unwrap();
        doc.layer_tree
            .extensions
            .insert("futureTree".into(), serde_json::json!(["tree"]));

        let path = path("nested-tree-roundtrip");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.layer_tree.selected_node_id, fill_id);
        assert_eq!(loaded.layer_tree.next_node_id, doc.layer_tree.next_node_id);
        assert_eq!(
            loaded.layer_tree.extensions["futureTree"],
            serde_json::json!(["tree"])
        );
        let LayerNode::Group(group) = &loaded.layer_tree.roots[1] else {
            panic!("expected root group");
        };
        assert_eq!(group.properties.id, group_id);
        assert_eq!(
            group.properties.extensions["futureGroup"],
            serde_json::json!({"retain": true})
        );
        let LayerNode::Paint(paint) = &group.children[0] else {
            panic!("expected nested paint node");
        };
        assert_eq!(paint.id, paint_id);
        let LayerNode::Fill(fill) = &group.children[1] else {
            panic!("expected nested fill node");
        };
        assert_eq!(fill.properties.opacity, 0.625);
        assert!(!fill.properties.visible);
        assert_eq!(fill.properties.mask.get_pixel(8, 9).0, [117]);
        assert_eq!(
            fill.properties.extensions["futureFill"],
            serde_json::json!("retained")
        );
        assert_eq!(fill.values[&Channel::BaseColor], [21, 43, 65, 255]);
        assert_eq!(fill.values[&Channel::Metallic], [128, 128, 128, 255]);

        save(&loaded, &path).unwrap();
        assert_eq!(saved_manifest(&path)["version"], 9);
        let resaved = load(&path).unwrap();
        assert_eq!(resaved.layer_tree.selected_node_id, fill_id);
        assert_eq!(resaved.layer_tree.next_node_id, doc.layer_tree.next_node_id);
        assert_eq!(
            resaved.layer_tree.extensions["futureTree"],
            serde_json::json!(["tree"])
        );
        let LayerNode::Group(resaved_group) = &resaved.layer_tree.roots[1] else {
            panic!("expected resaved root group");
        };
        assert_eq!(
            resaved_group.properties.extensions["futureGroup"],
            serde_json::json!({"retain": true})
        );
        let LayerNode::Fill(resaved_fill) = &resaved_group.children[1] else {
            panic!("expected resaved nested fill node");
        };
        assert_eq!(
            resaved_fill.properties.extensions["futureFill"],
            serde_json::json!("retained")
        );
        assert_eq!(
            resaved.layer_tree.composite(64, Channel::BaseColor),
            loaded.layer_tree.composite(64, Channel::BaseColor)
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn image_load_peak_budget_accounts_for_decoder_and_encoded_buffers() {
        let size = 64;
        let decode_scratch = u64::from(size) * u64::from(size) * 4;
        let encoded = 1024;
        let exact_retained =
            MAX_DECODED_BYTES - decode_scratch - encoded - SEED_DOCUMENT_MASK_BYTES;
        validate_image_load_peak(exact_retained, size, encoded).unwrap();
        let error = validate_image_load_peak(exact_retained + 1, size, encoded).unwrap_err();
        assert!(format!("{error:#}").contains("512-MiB peak image-loading budget"));
        assert!(validate_image_load_peak(0, u32::MAX, encoded).is_err());
    }

    #[test]
    fn loader_rejects_manifest_whose_image_staging_exceeds_peak_budget() {
        let path = path("image-load-peak-budget");
        save(&Document::new(64), &path).unwrap();
        rewrite_manifest(&path, |manifest| {
            manifest["size"] = serde_json::json!(MAX_SIZE);
            manifest["layer_tree"]["roots"][0]["channels"] = serde_json::json!(&Channel::ALL[..7]);
        });
        let error = match load(&path) {
            Ok(_) => panic!("image staging over the peak budget should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("512-MiB peak image-loading budget"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn streaming_v9_preflight_rejects_depth_count_and_decoded_budget() {
        let node = serde_json::json!({"kind":"paint","id":1,"name":"P","visible":true,
            "opacity":1.0,"blend":"Normal","disabled_channels":[],"channels":[]});
        let mut too_deep = node.clone();
        for id in (1..=32).rev() {
            too_deep = serde_json::json!({"kind":"group","id":id,"name":"G","visible":true,
                "opacity":1.0,"blend":"Normal","disabled_channels":[],"children":[too_deep]});
        }
        let make_manifest = |roots: serde_json::Value, size: u32| {
            serde_json::to_vec(&serde_json::json!({
                "version":9,"size":size,"layer_tree":{"selected_node_id":1,"next_node_id":5000,"roots":roots},
                "texture_sets":[]
            })).unwrap()
        };
        assert!(
            preflight_manifest_json(&make_manifest(serde_json::json!([too_deep]), 64)).is_err()
        );

        let mut oversized_name = node.clone();
        oversized_name["name"] = serde_json::json!("x".repeat(1025));
        assert!(
            preflight_manifest_json(&make_manifest(serde_json::json!([oversized_name]), 64))
                .is_err()
        );

        let many_nodes: Vec<_> = (1..=crate::layer_tree::MAX_LAYER_TREE_NODES + 1)
            .map(|id| {
                serde_json::json!({"kind":"paint","id":id,"name":"P","visible":true,
                    "opacity":1.0,"blend":"Normal","disabled_channels":[],"channels":[]})
            })
            .collect();
        assert!(
            preflight_manifest_json(&make_manifest(serde_json::json!(many_nodes), 64)).is_err()
        );

        let eight_channels = serde_json::json!({"kind":"paint","id":1,"name":"P","visible":true,
            "opacity":1.0,"blend":"Normal","disabled_channels":[],"channels":Channel::ALL});
        assert!(
            preflight_manifest_json(&make_manifest(serde_json::json!([eight_channels]), 4096))
                .is_err()
        );
    }

    #[test]
    fn streaming_preflight_rejects_high_amplification_extension_before_value_build() {
        let mut json = String::with_capacity(12 * 1024 * 1024);
        json.push_str(r#"{"version":9,"size":64,"layer_tree":{"selected_node_id":1,"next_node_id":2,"roots":[{"kind":"paint","id":1,"name":"P","visible":true,"opacity":1.0,"blend":"Normal","disabled_channels":[],"channels":[]}],"futureValues":["#);
        for index in 0..2_100_000 {
            if index != 0 {
                json.push(',');
            }
            json.push_str("true");
        }
        json.push_str(r#"]},"texture_sets":[]}"#);
        assert!(json.len() < 12 * 1024 * 1024);
        assert!(preflight_manifest_json(json.as_bytes()).is_err());
    }

    #[test]
    fn streaming_preflight_shares_extension_budget_across_manifest_scopes() {
        let cases = [
            r#"{"version":9,"future_root":false}"#,
            r#"{"version":9,"texture_sets":[{"size":64,"future_set":false}]}"#,
            r#"{"version":9,"layer_tree":{"roots":[{"kind":"paint","channels":[]}],"future_tree":false}}"#,
        ];
        for json in cases {
            let mut deserializer = serde_json::Deserializer::from_str(json);
            let budget = ExtensionBudget(Rc::new(Cell::new(MAX_MANIFEST_BYTES as usize - 80)));
            assert!(
                ManifestPreflightSeed(budget)
                    .deserialize(&mut deserializer)
                    .is_err(),
                "extension metadata was not charged across scopes: {json}"
            );
        }
    }
}

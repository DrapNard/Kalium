//! Native .kalium projects are versioned ZIP archives.
//! v4 adds mesh material bindings; v5 adds Detail Normal; v6 adds project identity.
//! Unknown ZIP data is rejected rather than silently discarded during a save.
use crate::document::{
    BlendMode, Channel, Document, Layer, TextureSet, MAX_DECODED_BYTES, MAX_LAYERS, MAX_SIZE,
    MAX_TEXTURE_SETS,
};
use crate::mesh::Mesh;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::File,
    io::{Cursor, Read, Write},
    path::Path,
};
use zip::{write::FileOptions, CompressionMethod, ZipArchive, ZipWriter};

const MAX_MANIFEST_BYTES: u64 = 48 * 1024 * 1024;
/// Upper bound for a project file whose bytes are hashed as a recovery baseline.
#[allow(dead_code)] // The S5 journal producer will consume this during app integration.
pub const MAX_PROJECT_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

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
    #[serde(flatten)]
    extensions: JsonMap<String, JsonValue>,
}

fn layer_manifests(layers: &[Layer]) -> Vec<LayerManifest> {
    layers
        .iter()
        .map(|l| LayerManifest {
            id: l.id,
            name: l.name.clone(),
            visible: l.visible,
            opacity: l.opacity,
            blend: l.blend,
            channels: Channel::ALL
                .into_iter()
                .filter(|c| l.pixels.contains_key(c))
                .collect(),
            disabled_channels: Channel::ALL
                .into_iter()
                .filter(|c| l.disabled_channels.contains(c))
                .collect(),
            extensions: l.extensions.clone(),
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

fn write_layers(
    writer: &mut ZipWriter<File>,
    opts: FileOptions,
    set_id: Option<u32>,
    layers: &[Layer],
) -> Result<()> {
    for layer in layers {
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
    }
    Ok(())
}

pub fn save(doc: &Document, path: &Path) -> Result<()> {
    save_with_replace(doc, path, |tmp, destination| {
        std::fs::rename(tmp, destination)
    })
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
/// durability; save uses the platform's ordinary file write and rename semantics.
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

fn save_with_replace<F>(doc: &Document, path: &Path, replace: F) -> Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    doc.validate()?;
    let manifest = Manifest {
        format: "Kalium Project".into(),
        version: 6,
        project_id: Some(doc.project_id),
        mesh: doc.mesh.clone(),
        size: doc.size,
        selected: doc.selected,
        next_layer_id: doc.next_layer_id,
        layers: layer_manifests(&doc.layers),
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
            .map(|set| SetManifest {
                id: set.id,
                name: set.name.clone(),
                size: set.size,
                selected: set.selected,
                next_layer_id: set.next_layer_id,
                layers: layer_manifests(&set.layers),
                extensions: set.extensions.clone(),
            })
            .collect(),
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
        let file = File::create(&tmp)?;
        temp_created = true;
        let mut writer = ZipWriter::new(file);
        let opts = FileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file("manifest.json", opts)?;
        writer.write_all(&manifest_bytes)?;
        write_layers(&mut writer, opts, None, &doc.layers)?;
        for set in &doc.texture_sets {
            write_layers(&mut writer, opts, Some(set.id), &set.layers)?;
        }
        writer.finish()?;
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
    Ok(())
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
        *total = (*total).saturating_add((size as u64).pow(2));
        let mut channels = HashSet::new();
        for &channel in &layer.channels {
            if !channels.insert(channel) {
                bail!("Duplicate channel in layer manifest");
            }
            valid.insert(image_name(set_id, layer.id, Some(channel)));
            *total = (*total).saturating_add((size as u64).pow(2) * 4);
        }
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
        layer.visible = meta.visible;
        layer.opacity = meta.opacity;
        layer.blend = meta.blend;
        layer.disabled_channels = meta.disabled_channels.into_iter().collect();
        layer.extensions = meta.extensions;
        let bytes = read_entry(
            zip,
            &image_name(set_id, meta.id, None),
            rgba_bytes + 4 * 1024 * 1024,
        )?;
        let mask_name = image_name(set_id, meta.id, None);
        layer.mask = decode_layer_image(&bytes, size, &mask_name)?.to_luma8();
        if layer.mask.dimensions() != (size, size) {
            bail!("Mask dimensions do not match project");
        }
        for channel in meta.channels {
            let channel_name = image_name(set_id, meta.id, Some(channel));
            let bytes = read_entry(zip, &channel_name, rgba_bytes + 4 * 1024 * 1024)?;
            let rgba = decode_layer_image(&bytes, size, &channel_name)?.to_rgba8();
            if rgba.dimensions() != (size, size) {
                bail!("Channel dimensions do not match project");
            }
            layer.pixels.insert(channel, rgba);
        }
        result.push(layer);
    }
    Ok(result)
}

pub fn load(path: &Path) -> Result<Document> {
    let file = File::open(path)?;
    let mut zip = ZipArchive::new(file).context("Not a valid Kalium project ZIP")?;
    let limit = 1 + MAX_TEXTURE_SETS * MAX_LAYERS * (Channel::ALL.len() + 2);
    if zip.len() > limit {
        bail!("Project contains too many ZIP entries");
    }
    let manifest: Manifest =
        serde_json::from_slice(&read_entry(&mut zip, "manifest.json", MAX_MANIFEST_BYTES)?)?;
    if manifest.format != "Kalium Project" || !matches!(manifest.version, 1..=6) {
        bail!("Unsupported Kalium project version");
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
    manifest.mesh.validate()?;
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
    for id in manifest.material_bindings.iter().flatten() {
        if *id != manifest.active_set_id && !manifest.texture_sets.iter().any(|set| set.id == *id) {
            bail!("Material binding references missing texture set {id}");
        }
    }
    let mut valid = HashSet::from(["manifest.json".to_owned()]);
    let mut decoded_bytes = 0;
    account_layers(
        &mut valid,
        &mut decoded_bytes,
        None,
        manifest.size,
        &manifest.layers,
    )?;
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
            account_layers(
                &mut valid,
                &mut decoded_bytes,
                Some(set.id),
                set.size,
                &set.layers,
            )?;
        }
    }
    if decoded_bytes > MAX_DECODED_BYTES {
        bail!("Project exceeds 512-MiB decoded texture budget");
    }
    let mut seen = HashSet::new();
    for idx in 0..zip.len() {
        let entry = zip.by_index(idx)?;
        if !seen.insert(entry.name().to_owned()) {
            bail!(
                "Duplicate ZIP entry '{}'; refusing ambiguous input",
                entry.name()
            );
        }
        if entry.is_dir() {
            let name = entry.name().trim_end_matches('/');
            let is_parent = valid
                .iter()
                .any(|path| path.starts_with(&format!("{name}/")));
            if !is_parent {
                bail!("Unexpected ZIP directory '{}'", entry.name());
            }
        } else if !valid.contains(entry.name()) {
            bail!(
                "Unknown archive entry '{}'; refusing lossy import",
                entry.name()
            );
        }
    }
    let mut doc = Document::new(manifest.size);
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
    doc.selected = manifest.selected;
    doc.next_layer_id = manifest.next_layer_id;
    doc.extensions = manifest.extensions;
    doc.layers = load_layers(&mut zip, None, manifest.size, manifest.layers)?;
    if manifest.version >= 3 {
        doc.active_set_id = manifest.active_set_id;
        doc.active_set_name = manifest.active_set_name;
        doc.next_set_id = manifest.next_set_id;
        for set in manifest.texture_sets {
            let layers = load_layers(&mut zip, Some(set.id), set.size, set.layers)?;
            doc.texture_sets.push(TextureSet {
                id: set.id,
                name: set.name,
                size: set.size,
                layers,
                selected: set.selected,
                next_layer_id: set.next_layer_id,
                extensions: set.extensions,
            });
        }
    }
    doc.validate()?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{export, paint::Brush, uv::UvCoverage};
    use image::{Luma, Rgba};
    use std::path::PathBuf;

    struct Cleanup(Vec<PathBuf>);

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

    fn rewrite_manifest(path: &Path, mut update: impl FnMut(&mut serde_json::Value)) {
        let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_owned();
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
        let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        let rewritten = writer.finish().unwrap().into_inner();
        std::fs::write(path, rewritten).unwrap();
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
            let name = entry.name().to_owned();
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
        let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
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
            let entry_name = entry.name().to_owned();
            let mut entry_bytes = Vec::new();
            entry.read_to_end(&mut entry_bytes).unwrap();
            entries.push((entry_name, entry_bytes));
        }
        drop(archive);

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
        for (entry_name, entry_bytes) in entries {
            writer.start_file(entry_name, options).unwrap();
            writer.write_all(&entry_bytes).unwrap();
        }
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
        let rewritten = writer.finish().unwrap().into_inner();
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
        doc.selected_layer_mut().mask.put_pixel(12, 12, Luma([70]));
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(11, 10, Rgba([12, 34, 56, 200]));
        doc.selected_layer_mut()
            .disabled_channels
            .insert(Channel::BaseColor);
        let path = path("roundtrip");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.layers.len(), 2);
        assert!(loaded
            .selected_layer()
            .disabled_channels
            .contains(&Channel::BaseColor));
        assert_eq!(loaded.layers[1].mask.get_pixel(12, 12).0, [70]);
        assert_eq!(
            loaded.layers[1].pixels[&Channel::BaseColor]
                .get_pixel(11, 10)
                .0,
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
        original.layers[1].name = "Checkpoint detail".into();
        original.layers[1].mask.put_pixel(7, 9, Luma([123]));
        original.layers[1]
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(7, 9, Rgba([31, 62, 93, 187]));
        original.layers[1].extensions.insert(
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
        assert_eq!(checkpoint_loaded.layers.len(), 1);
        assert_eq!(
            checkpoint_loaded.selected_layer().pixels[&Channel::Normal]
                .get_pixel(2, 4)
                .0,
            [128, 128, 255, 255]
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0].layers[1]
                .mask
                .get_pixel(7, 9)
                .0,
            [123]
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0].layers[1].pixels[&Channel::BaseColor]
                .get_pixel(7, 9)
                .0,
            [31, 62, 93, 187]
        );
        assert_eq!(
            checkpoint_loaded.texture_sets[0].layers[1].extensions["future_layer_field"],
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
            |manifest| manifest["layers"][0]["opacity"] = serde_json::json!(2.0),
            ("layers/1/mask.png", b"invalid image payload"),
        );

        let error = match load(&path) {
            Ok(_) => panic!("invalid metadata should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("Invalid opacity"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_duplicate_zip_entry_names() {
        let doc = Document::new(64);
        let path = path("duplicate-entry");
        save(&doc, &path).unwrap();
        let manifest = saved_manifest(&path);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        add_archive_entry(&path, "manifest.json", &bytes);

        let error = match load(&path) {
            Ok(_) => panic!("duplicate ZIP entries should be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("Duplicate ZIP entry"));
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
            manifest["layers"][0]["channels"] = serde_json::json!(Channel::ALL);
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
    fn multiple_texture_sets_roundtrip_with_distinct_layers() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(0, 0, Rgba([1, 2, 3, 255]));
        let id = doc.add_texture_set("Armor / Leather").unwrap();
        doc.try_resize(128).unwrap();
        doc.selected_layer_mut().mask.put_pixel(1, 1, Luma([64]));
        doc.selected_layer_mut()
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
        assert_eq!(restored.selected_layer().mask.get_pixel(1, 1).0, [64]);
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
    fn mesh_material_bindings_roundtrip_v6() {
        let mut doc = Document::new(64);
        let second = doc.add_texture_set("Leather").unwrap();
        doc.mesh.materials.push("Glove".into());
        doc.mesh.triangle_materials[0] = 1;
        doc.material_bindings = vec![Some(second), None];
        let path = path("material-bindings");
        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(loaded.material_bindings, vec![Some(second), None]);
        assert_eq!(loaded.mesh.triangle_material(0), 1);
        assert_eq!(loaded.mesh.material_name(1), "Glove");
    }

    #[test]
    fn v4_project_loads_and_saves_as_v6_with_stable_identity() {
        let mut doc = Document::new(64);
        doc.mesh.materials.push("Second material".into());
        doc.material_bindings = vec![None, None];
        let path = path("v4-to-v5");
        save(&doc, &path).unwrap();
        rewrite_manifest(&path, |manifest| {
            manifest["version"] = serde_json::json!(4);
            manifest.as_object_mut().unwrap().remove("project_id");
        });

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.material_bindings, vec![None, None]);
        let migrated_id = loaded.project_id;
        assert!(!migrated_id.is_nil());
        save(&loaded, &path).unwrap();
        let manifest = saved_manifest(&path);
        assert_eq!(manifest["version"], 6);
        assert_eq!(manifest["project_id"], migrated_id.to_string());
        assert_eq!(load(&path).unwrap().project_id, migrated_id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detail_normal_blend_roundtrips_in_v6_manifest() {
        let mut doc = Document::new(64);
        doc.layers[0].blend = BlendMode::Detail;
        let path = path("detail-normal-v6");
        save(&doc, &path).unwrap();
        let manifest = saved_manifest(&path);
        assert_eq!(manifest["version"], 6);
        assert_eq!(manifest["layers"][0]["blend"], "Detail");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.layers[0].blend, BlendMode::Detail);
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
            object.get_mut("layers").unwrap()[0]["futureLayer"] = layer_extension.clone();
            object.get_mut("mesh").unwrap()["futureMesh"] = mesh_extension.clone();
            object.get_mut("mesh").unwrap()["vertices"][0]["futureVertex"] =
                vertex_extension.clone();
        });

        let mut loaded = load(&path).unwrap();
        assert_eq!(loaded.extensions.get("futureRoot"), Some(&root_extension));
        assert_eq!(
            loaded.texture_sets[0].extensions.get("futureSet"),
            Some(&set_extension)
        );
        assert_eq!(
            loaded.layers[0].extensions.get("futureLayer"),
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
            reopened.texture_sets[0].extensions.get("futureSet"),
            Some(&set_extension)
        );
        assert_eq!(
            reopened.layers[0].extensions.get("futureLayer"),
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
        assert!(doc.layers.iter().all(|l| l.disabled_channels.is_empty()));
        assert_eq!(doc.texture_set_count(), 1);
        assert_eq!(doc.active_set_id, 1);
        assert!(!doc.project_id.is_nil());
        doc.validate().unwrap();
    }

    #[test]
    fn v6_rejects_nil_project_identity() {
        let path = path("nil-project-id");
        save(&Document::new(64), &path).unwrap();
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
}

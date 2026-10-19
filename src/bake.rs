//! Bounded, deterministic CPU mesh-map baking primitives.
//!
//! The module rasterizes mesh attributes and ambient occlusion into a Texture
//! Set's UV atlas. AO can run on a cancellable worker and be installed as an
//! editable Occlusion layer; bounded per-Texture-Set bake settings persist as
//! project metadata, while high-to-low outputs are not stored as mesh maps.
use crate::mesh::Mesh;
use anyhow::{bail, Context, Result};
use glam::{DVec2, DVec3, Vec3};
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
};

static MATERIAL_ID_OUTPUT_NONCE: AtomicU64 = AtomicU64::new(0);

/// Build the stable sidecar metadata shared by CLI and editor material-ID exports.
pub fn material_id_metadata(
    mesh: &Mesh,
    size: u32,
    set_id: u32,
    set_name: &str,
    active_in_source_project: bool,
) -> Result<serde_json::Value> {
    if set_id == 0 || !(64..=MAX_BAKE_SIZE).contains(&size) {
        bail!("Invalid material-ID metadata Texture Set or dimensions");
    }
    let mut material_slots = Vec::new();
    material_slots
        .try_reserve_exact(mesh.material_count())
        .map_err(|_| anyhow::anyhow!("Could not reserve material-ID slot metadata"))?;
    for slot in 0..mesh.material_count() {
        material_slots.push(serde_json::json!({
            "slot_index": slot,
            "name": mesh.material_name(slot),
        }));
    }
    Ok(serde_json::json!({
        "format": "Kalium Material ID",
        "version": 1,
        "encoding": {
            "channel": "RGB",
            "slot_index_bits": 24,
            "byte_order": "little-endian",
            "rgb": ["slot_index & 0xff", "(slot_index >> 8) & 0xff", "(slot_index >> 16) & 0xff"],
            "alpha": {"covered": 255, "uncovered": 0},
            "uncovered_rgb": [0, 0, 0],
        },
        "dimensions": {"width": size, "height": size},
        "texture_set": {
            "id": set_id,
            "name": set_name,
            "active_in_source_project": active_in_source_project,
        },
        "uv_overlap_policy": "first covering triangle in mesh index order",
        "material_slots": material_slots,
    }))
}

/// Write a material-ID PNG and its metadata sidecar as one recoverable pair.
/// Both files are staged and synced first; if publishing either one fails, any
/// previous pair is restored and the newly published half is removed.
pub fn write_material_id_outputs(
    image: &RgbaImage,
    metadata: &serde_json::Value,
    output_path: &Path,
) -> Result<PathBuf> {
    use image::ImageEncoder;

    if output_path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("png")
    {
        bail!("Material-ID output path must end in .png");
    }
    let mut sidecar_name = output_path.as_os_str().to_os_string();
    sidecar_name.push(".json");
    let sidecar_path = PathBuf::from(sidecar_name);
    let (temp_png, temp_json) = unique_material_id_temp_paths(output_path);

    let write_result = (|| -> Result<()> {
        let png_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_png)
            .context("Could not create staged material-ID PNG")?;
        image::codecs::png::PngEncoder::new(png_file).write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )?;
        File::open(&temp_png)?.sync_all()?;

        let json = serde_json::to_vec_pretty(metadata)?;
        let mut json_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_json)
            .context("Could not create staged material-ID metadata")?;
        json_file.write_all(&json)?;
        json_file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_png);
        let _ = fs::remove_file(&temp_json);
        return Err(error);
    }

    let backup_png = unique_material_id_temp_paths(output_path).0;
    let backup_json = unique_material_id_temp_paths(&sidecar_path).0;
    let mut png_backed_up = false;
    let mut json_backed_up = false;
    let backup_result = (|| -> Result<()> {
        for (path, backup, backed_up) in [
            (output_path, backup_png.as_path(), &mut png_backed_up),
            (
                sidecar_path.as_path(),
                backup_json.as_path(),
                &mut json_backed_up,
            ),
        ] {
            if path.exists() {
                if !path.is_file() {
                    bail!(
                        "Material-ID output path is not a regular file: {}",
                        path.display()
                    );
                }
                fs::rename(path, backup).with_context(|| {
                    format!("Could not preserve existing output {}", path.display())
                })?;
                *backed_up = true;
            }
        }
        Ok(())
    })();
    if let Err(error) = backup_result {
        if png_backed_up {
            let _ = fs::rename(&backup_png, output_path);
        }
        let _ = fs::remove_file(&temp_png);
        let _ = fs::remove_file(&temp_json);
        return Err(error);
    }

    let publish_result = fs::rename(&temp_png, output_path)
        .with_context(|| {
            format!(
                "Could not publish material-ID PNG {}",
                output_path.display()
            )
        })
        .and_then(|()| {
            fs::rename(&temp_json, &sidecar_path).with_context(|| {
                format!(
                    "Could not publish material-ID sidecar {}",
                    sidecar_path.display()
                )
            })
        });
    if let Err(error) = publish_result {
        let _ = fs::remove_file(output_path);
        let _ = fs::remove_file(&sidecar_path);
        if png_backed_up {
            let _ = fs::rename(&backup_png, output_path);
        }
        if json_backed_up {
            let _ = fs::rename(&backup_json, &sidecar_path);
        }
        let _ = fs::remove_file(&temp_png);
        let _ = fs::remove_file(&temp_json);
        return Err(error);
    }

    if png_backed_up {
        let _ = fs::remove_file(backup_png);
    }
    if json_backed_up {
        let _ = fs::remove_file(backup_json);
    }
    Ok(sidecar_path)
}

/// Stage and publish a high-to-low world-normal, coverage, and metadata bundle.
/// Existing outputs are restored if any publish step fails.
pub fn write_high_to_low_outputs(
    maps: &BakedMeshMaps,
    metadata: &serde_json::Value,
    output_dir: &Path,
) -> Result<[PathBuf; 3]> {
    use image::ImageEncoder;

    fs::create_dir_all(output_dir)?;
    let outputs = [
        output_dir.join("Kalium_HighToLow_WorldNormal.png"),
        output_dir.join("Kalium_HighToLow_Coverage.png"),
        output_dir.join("Kalium_HighToLow_Metadata.json"),
    ];
    let nonce = MATERIAL_ID_OUTPUT_NONCE.fetch_add(1, Ordering::Relaxed);
    let temporary: [PathBuf; 3] = std::array::from_fn(|i| {
        output_dir.join(format!(
            ".kalium-high-to-low-{}-{nonce}-{i}.tmp",
            std::process::id()
        ))
    });
    let backups: [PathBuf; 3] = std::array::from_fn(|i| {
        output_dir.join(format!(
            ".kalium-high-to-low-{}-{nonce}-{i}.bak",
            std::process::id()
        ))
    });
    let stage_result = (|| -> Result<()> {
        let normal_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary[0])?;
        image::codecs::png::PngEncoder::new(normal_file).write_image(
            maps.world_normal_image().as_raw(),
            maps.size,
            maps.size,
            image::ExtendedColorType::Rgba8,
        )?;
        File::open(&temporary[0])?.sync_all()?;

        let coverage: Vec<u8> = maps
            .valid
            .iter()
            .map(|&valid| if valid == 0 { 0 } else { 255 })
            .collect();
        let coverage_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary[1])?;
        image::codecs::png::PngEncoder::new(coverage_file).write_image(
            &coverage,
            maps.size,
            maps.size,
            image::ExtendedColorType::L8,
        )?;
        File::open(&temporary[1])?.sync_all()?;

        let mut metadata_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary[2])?;
        metadata_file.write_all(&serde_json::to_vec_pretty(metadata)?)?;
        metadata_file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = stage_result {
        for path in &temporary {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }

    let mut backed_up = [false; 3];
    for i in 0..outputs.len() {
        if outputs[i].exists() {
            if !outputs[i].is_file() {
                for path in &temporary {
                    let _ = fs::remove_file(path);
                }
                for j in 0..i {
                    if backed_up[j] {
                        let _ = fs::rename(&backups[j], &outputs[j]);
                    }
                }
                bail!(
                    "High-to-low output path is not a regular file: {}",
                    outputs[i].display()
                );
            }
            if let Err(error) = fs::rename(&outputs[i], &backups[i]) {
                for j in 0..i {
                    if backed_up[j] {
                        let _ = fs::rename(&backups[j], &outputs[j]);
                    }
                }
                for path in &temporary {
                    let _ = fs::remove_file(path);
                }
                return Err(error).with_context(|| {
                    format!(
                        "Could not preserve existing output {}",
                        outputs[i].display()
                    )
                });
            }
            backed_up[i] = true;
        }
    }

    for i in 0..outputs.len() {
        if let Err(error) = fs::rename(&temporary[i], &outputs[i]) {
            for j in 0..outputs.len() {
                if outputs[j].exists() && !backed_up[j] {
                    let _ = fs::remove_file(&outputs[j]);
                }
                if backed_up[j] {
                    let _ = fs::remove_file(&outputs[j]);
                    let _ = fs::rename(&backups[j], &outputs[j]);
                }
                let _ = fs::remove_file(&temporary[j]);
            }
            return Err(error).with_context(|| {
                format!(
                    "Could not publish high-to-low output {}",
                    outputs[i].display()
                )
            });
        }
    }
    for i in 0..outputs.len() {
        if backed_up[i] {
            let _ = fs::remove_file(&backups[i]);
        }
    }
    Ok(outputs)
}

fn unique_material_id_temp_paths(path: &Path) -> (PathBuf, PathBuf) {
    let nonce = MATERIAL_ID_OUTPUT_NONCE.fetch_add(1, Ordering::Relaxed);
    let mut first = path.as_os_str().to_os_string();
    first.push(format!(".kalium-{}-{nonce}.tmp", std::process::id()));
    let mut second = first.clone();
    second.push(".sidecar");
    (PathBuf::from(first), PathBuf::from(second))
}

const MAX_BAKE_SIZE: u32 = 4096;
const MAX_RASTER_SAMPLES: u64 = 90_000_000;
pub const MAX_AO_SAMPLES_PER_TEXEL: u32 = 4096;
pub const MAX_AO_RAYS: u64 = 100_000_000;
pub const AO_SETTINGS_EXTENSION_KEY: &str = "kalium_bake_ao";
const BVH_NODE_LEAF_TRIANGLES: usize = 8;

/// Per-Texture-Set options for a deterministic ambient-occlusion bake.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AmbientOcclusionBakeSettings {
    pub sample_count: u32,
    pub max_ray_distance: f32,
    pub seed: u64,
}

impl Default for AmbientOcclusionBakeSettings {
    fn default() -> Self {
        Self {
            sample_count: 16,
            max_ray_distance: 1.0,
            seed: 0,
        }
    }
}

impl AmbientOcclusionBakeSettings {
    pub fn sample_limit(size: u32) -> u32 {
        let texels = u64::from(size).saturating_mul(u64::from(size)).max(1);
        (MAX_AO_RAYS / texels)
            .min(u64::from(MAX_AO_SAMPLES_PER_TEXEL))
            .max(1) as u32
    }

    pub fn clamped_for_size(mut self, size: u32) -> Self {
        self.sample_count = self.sample_count.clamp(1, Self::sample_limit(size));
        if !self.max_ray_distance.is_finite() {
            self.max_ray_distance = Self::default().max_ray_distance;
        }
        self.max_ray_distance = self.max_ray_distance.clamp(0.01, 100.0);
        self
    }

    pub fn validate_for_size(self, size: u32) -> Result<()> {
        if !(1..=Self::sample_limit(size)).contains(&self.sample_count) {
            bail!("Ambient-occlusion sample count exceeds the texture-size ray budget");
        }
        if !self.max_ray_distance.is_finite() || !(0.01..=100.0).contains(&self.max_ray_distance) {
            bail!("Ambient-occlusion ray distance is outside the supported range");
        }
        Ok(())
    }
}
const RAY_ORIGIN_BIAS: f64 = 1.0e-7;

/// CPU-baked per-texel attributes. `valid` is zero outside covered UVs.
/// Position values use the imported mesh's coordinate space; normals are
/// normalized world-space directions, not tangent-space normal-map values.
#[derive(Debug)]
pub struct BakedMeshMaps {
    pub size: u32,
    pub positions: Vec<[f32; 3]>,
    pub world_normals: Vec<[f32; 3]>,
    pub valid: Vec<u8>,
    pub position_min: [f32; 3],
    pub position_max: [f32; 3],
}

/// Controls a high-to-low projection whose meshes already share a coordinate space.
pub struct HighToLowBakeOptions<'a> {
    pub size: u32,
    pub bindings: &'a [Option<u32>],
    pub active_set_id: u32,
    pub max_ray_distance: f32,
    pub working_budget_bytes: u64,
}

/// Controls a deterministic, cosine-weighted self-occlusion bake.
#[derive(Clone, Copy)]
pub struct AmbientOcclusionBakeOptions<'a> {
    pub size: u32,
    pub bindings: &'a [Option<u32>],
    pub active_set_id: u32,
    /// Number of cosine-weighted hemisphere rays per covered destination texel.
    /// Must be in `1..=4096`.
    pub sample_count: u32,
    /// Rays that do not hit within this distance contribute zero occlusion.
    pub max_ray_distance: f32,
    /// Seeds a per-texel SplitMix64 sequence; identical inputs produce identical maps.
    pub seed: u64,
    /// Maximum transient memory budget, including intermediate maps and the BVH.
    pub working_budget_bytes: u64,
}

/// Shared options for SubCraft-derived curvature and thickness mesh maps.
/// Distances are in imported mesh units. Material bindings filter destination
/// texels only; the whole mesh supplies neighborhood geometry and ray hits.
#[derive(Clone, Copy)]
pub struct SurfaceMapBakeOptions<'a> {
    pub size: u32,
    pub bindings: &'a [Option<u32>],
    pub active_set_id: u32,
    pub max_ray_distance: f32,
    pub curvature_radius: f32,
    pub sample_count: u32,
    pub seed: u64,
    pub working_budget_bytes: u64,
}

/// Owned settings for a background ambient-occlusion bake.
///
/// The worker owns these settings and its mesh snapshot, so it never needs a
/// document lock or references into UI state.
#[derive(Clone)]
pub struct AmbientOcclusionBakeJobOptions {
    pub size: u32,
    pub bindings: Vec<Option<u32>>,
    pub active_set_id: u32,
    pub sample_count: u32,
    pub max_ray_distance: f32,
    pub seed: u64,
    pub working_budget_bytes: u64,
}

/// Owned options for a cancellable material-ID bake.
#[derive(Clone)]
pub struct MaterialIdBakeJobOptions {
    pub size: u32,
    pub bindings: Vec<Option<u32>>,
    pub active_set_id: u32,
    pub working_budget_bytes: u64,
}

/// Cancellable background material-ID bake. Poll [`Self::try_take_result`]
/// from the UI thread; dropping the handle requests cancellation.
pub struct MaterialIdBakeWorker {
    cancel: Arc<AtomicBool>,
    result_rx: Receiver<Result<RgbaImage>>,
    _worker: JoinHandle<()>,
    result_taken: bool,
}

impl MaterialIdBakeWorker {
    pub fn spawn(mesh: Mesh, options: MaterialIdBakeJobOptions) -> Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("kalium-material-id-bake".into())
            .spawn(move || {
                let result = bake_material_id(
                    &mesh,
                    options.size,
                    &options.bindings,
                    options.active_set_id,
                    options.working_budget_bytes,
                    &worker_cancel,
                );
                let _ = result_tx.send(result);
            })
            .context("Could not start material-ID bake worker")?;

        Ok(Self {
            cancel,
            result_rx,
            _worker: worker,
            result_taken: false,
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn try_take_result(&mut self) -> Option<Result<RgbaImage>> {
        if self.result_taken {
            return None;
        }
        match self.result_rx.try_recv() {
            Ok(result) => {
                self.result_taken = true;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.result_taken = true;
                Some(Err(anyhow::anyhow!(
                    "Material-ID bake worker stopped without a result"
                )))
            }
        }
    }
}

impl Drop for MaterialIdBakeWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Monotonic progress for the ambient-occlusion worker's sampling pass.
/// Earlier UV-raster and acceleration phases report zero completed rows; the
/// total remains the requested bake size throughout the job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmbientOcclusionBakeProgress {
    pub completed_rows: u32,
    pub total_rows: u32,
}

/// Cancellable background AO bake. Poll [`Self::progress`] and
/// [`Self::try_take_result`] from the UI thread; neither call waits for the
/// worker. Dropping the handle requests cancellation.
pub struct AmbientOcclusionBakeWorker {
    cancel: Arc<AtomicBool>,
    completed_rows: Arc<AtomicU32>,
    total_rows: u32,
    result_rx: Receiver<Result<BakedAmbientOcclusion>>,
    // Keeping the handle documents ownership; dropping it detaches the worker,
    // which observes `cancel` at the normal AO cancellation checkpoints.
    _worker: JoinHandle<()>,
    result_taken: bool,
}

impl AmbientOcclusionBakeWorker {
    /// Start a background bake with an owned mesh snapshot and owned options.
    pub fn spawn(mesh: Mesh, options: AmbientOcclusionBakeJobOptions) -> Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let completed_rows = Arc::new(AtomicU32::new(0));
        let total_rows = options.size;
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker_cancel = Arc::clone(&cancel);
        let worker_rows = Arc::clone(&completed_rows);
        let worker = thread::Builder::new()
            .name("kalium-ao-bake".into())
            .spawn(move || {
                let result = bake_ambient_occlusion_with_progress(
                    &mesh,
                    AmbientOcclusionBakeOptions {
                        size: options.size,
                        bindings: &options.bindings,
                        active_set_id: options.active_set_id,
                        sample_count: options.sample_count,
                        max_ray_distance: options.max_ray_distance,
                        seed: options.seed,
                        working_budget_bytes: options.working_budget_bytes,
                    },
                    &worker_cancel,
                    |rows| worker_rows.store(rows, Ordering::Relaxed),
                );
                let _ = result_tx.send(result);
            })
            .context("Could not start ambient-occlusion bake worker")?;

        Ok(Self {
            cancel,
            completed_rows,
            total_rows,
            result_rx,
            _worker: worker,
            result_taken: false,
        })
    }

    pub fn progress(&self) -> AmbientOcclusionBakeProgress {
        AmbientOcclusionBakeProgress {
            completed_rows: self.completed_rows.load(Ordering::Relaxed),
            total_rows: self.total_rows,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Return the completed result if available. The result is delivered once;
    /// a disconnected worker is surfaced as an error instead of waiting.
    pub fn try_take_result(&mut self) -> Option<Result<BakedAmbientOcclusion>> {
        if self.result_taken {
            return None;
        }
        match self.result_rx.try_recv() {
            Ok(result) => {
                self.result_taken = true;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.result_taken = true;
                Some(Err(anyhow::anyhow!(
                    "Ambient-occlusion bake worker stopped without a result"
                )))
            }
        }
    }
}

impl Drop for AmbientOcclusionBakeWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Per-texel ambient occlusion. Valid texels contain the fraction of sampled
/// hemisphere rays that hit the source mesh; 0 is open and 1 is fully occluded.
/// Invalid texels are outside the active Texture Set's UV coverage.
#[derive(Debug)]
pub struct BakedAmbientOcclusion {
    pub size: u32,
    pub occlusion: Vec<f32>,
    pub valid: Vec<u8>,
}

impl BakedAmbientOcclusion {
    #[cfg(test)]
    pub fn is_valid(&self, x: u32, y: u32) -> bool {
        x < self.size && y < self.size && self.valid[(y * self.size + x) as usize] != 0
    }

    /// Encode this bake as an RGBA Occlusion-channel image: white is open,
    /// black is fully occluded, and uncovered UV texels are transparent white.
    /// Finite values outside `[0, 1]` are clamped. A non-finite value in a
    /// covered texel is treated as zero occlusion (open white) so malformed
    /// public data cannot create NaN-dependent or spuriously dark output; the
    /// bake algorithms themselves produce finite values in `[0, 1]`.
    pub fn occlusion_image(&self) -> Result<RgbaImage> {
        if !(64..=MAX_BAKE_SIZE).contains(&self.size) {
            bail!("Ambient occlusion image dimension must be between 64 and {MAX_BAKE_SIZE}");
        }
        let texel_count = u64::from(self.size)
            .checked_mul(u64::from(self.size))
            .context("Ambient occlusion image dimensions overflow")?
            as usize;
        if self.occlusion.len() != texel_count || self.valid.len() != texel_count {
            bail!("Ambient occlusion data does not match its image dimensions");
        }

        let mut image = RgbaImage::new(self.size, self.size);
        for (index, pixel) in image.pixels_mut().enumerate() {
            if self.valid[index] == 0 {
                *pixel = Rgba([255, 255, 255, 0]);
                continue;
            }
            let occlusion = self.occlusion[index];
            let occlusion = if occlusion.is_finite() {
                occlusion.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let open = (1.0 - occlusion) * 255.0;
            let value = open.round() as u8;
            *pixel = Rgba([value, value, value, 255]);
        }
        Ok(image)
    }
}

impl BakedMeshMaps {
    pub fn is_valid(&self, x: u32, y: u32) -> bool {
        x < self.size && y < self.size && self.valid[(y * self.size + x) as usize] != 0
    }

    #[cfg(test)]
    fn position(&self, x: u32, y: u32) -> Option<[f32; 3]> {
        self.is_valid(x, y)
            .then(|| self.positions[(y * self.size + x) as usize])
    }

    #[cfg(test)]
    fn world_normal(&self, x: u32, y: u32) -> Option<[f32; 3]> {
        self.is_valid(x, y)
            .then(|| self.world_normals[(y * self.size + x) as usize])
    }

    pub fn covered_texels(&self) -> usize {
        self.valid.iter().filter(|&&value| value != 0).count()
    }

    /// Encode world-space directions using the common signed-normal RGB map
    /// convention. Uncovered texels are transparent neutral normals.
    pub fn world_normal_image(&self) -> RgbaImage {
        let mut image = RgbaImage::new(self.size, self.size);
        for (index, pixel) in image.pixels_mut().enumerate() {
            if self.valid[index] == 0 {
                *pixel = Rgba([128, 128, 255, 0]);
                continue;
            }
            let normal = self.world_normals[index];
            let encode =
                |component: f32| ((component.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round() as u8;
            *pixel = Rgba([encode(normal[0]), encode(normal[1]), encode(normal[2]), 255]);
        }
        image
    }

    /// Encode position as normalized mesh-bounds RGB. The bake result retains
    /// the bounds so the mapping can be reversed exactly up to 8-bit output.
    pub fn normalized_position_image(&self) -> RgbaImage {
        let mut image = RgbaImage::new(self.size, self.size);
        for (index, pixel) in image.pixels_mut().enumerate() {
            if self.valid[index] == 0 {
                *pixel = Rgba([0, 0, 0, 0]);
                continue;
            }
            let position = Vec3::from_array(self.positions[index]);
            let normalized = [
                normalize_position_axis(position.x, self.position_min[0], self.position_max[0]),
                normalize_position_axis(position.y, self.position_min[1], self.position_max[1]),
                normalize_position_axis(position.z, self.position_min[2], self.position_max[2]),
            ];
            *pixel = Rgba([
                (normalized[0] * 255.0).round() as u8,
                (normalized[1] * 255.0).round() as u8,
                (normalized[2] * 255.0).round() as u8,
                255,
            ]);
        }
        image
    }
}

fn normalize_position_axis(value: f32, minimum: f32, maximum: f32) -> f32 {
    let extent = f64::from(maximum) - f64::from(minimum);
    if extent.abs() <= 1.0e-12 {
        0.5
    } else {
        ((f64::from(value) - f64::from(minimum)) / extent).clamp(0.0, 1.0) as f32
    }
}

/// Bake while respecting the caller's remaining transient-memory budget.
/// The estimate includes float output maps and one encoded RGBA image during
/// file writing.
pub fn bake_mesh_maps_budgeted(
    mesh: &Mesh,
    size: u32,
    bindings: &[Option<u32>],
    active_set_id: u32,
    working_budget_bytes: u64,
    cancel: &AtomicBool,
) -> Result<BakedMeshMaps> {
    bake_mesh_maps_with_cancel(
        mesh,
        size,
        bindings,
        active_set_id,
        working_budget_bytes,
        || cancel.load(Ordering::Relaxed),
    )
}

/// Bake the destination mesh's material slot IDs into its UV atlas.
///
/// Covered texels contain the exact 24-bit `Mesh::triangle_material` slot
/// index in little-endian RGB order (`slot & 0xff`, `slot >> 8`,
/// `slot >> 16`) and alpha 255. Uncovered texels are transparent black.
/// Texture Set bindings filter destination triangles; unbound slots belong to
/// `active_set_id`. Raster order is mesh order, so the first triangle owns UV
/// overlaps. The output and raster work are bounded by the same limits as the
/// other CPU bake operations.
pub fn bake_material_id(
    mesh: &Mesh,
    size: u32,
    bindings: &[Option<u32>],
    active_set_id: u32,
    working_budget_bytes: u64,
    cancel: &AtomicBool,
) -> Result<RgbaImage> {
    if cancel.load(Ordering::Relaxed) {
        bail!("Material-ID bake canceled");
    }
    mesh.validate()?;
    if !(64..=MAX_BAKE_SIZE).contains(&size) {
        bail!("Material-ID bake dimension must be between 64 and {MAX_BAKE_SIZE}");
    }
    if active_set_id == 0 {
        bail!("Active Texture Set ID must be nonzero");
    }
    if bindings.len() != mesh.material_count() {
        bail!("Material binding count does not match mesh slots");
    }
    if bindings.iter().flatten().any(|id| *id == 0) {
        bail!("Texture Set binding IDs must be nonzero");
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("Material-ID bake canceled");
    }

    let texel_count = u64::from(size)
        .checked_mul(u64::from(size))
        .context("Material-ID bake dimensions overflow")?;
    let output_bytes = texel_count
        .checked_mul(4)
        .context("Material-ID bake working size overflow")?;
    if output_bytes > working_budget_bytes {
        bail!("Material-ID bake exceeds the caller's transient-memory budget");
    }
    let output_len = usize::try_from(output_bytes).context("Material-ID image size overflow")?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(output_len)
        .context("Could not reserve material-ID bake output memory")?;
    pixels.resize(output_len, 0);
    let scale = f64::from(size - 1);
    let mut raster_samples = 0u64;

    for (triangle_index, triangle) in mesh.indices.chunks_exact(3).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Material-ID bake canceled");
        }
        let slot = mesh.triangle_material(triangle_index);
        let assigned_set = bindings
            .get(slot)
            .copied()
            .flatten()
            .unwrap_or(active_set_id);
        if assigned_set != active_set_id {
            continue;
        }

        let vertices = [
            &mesh.vertices[triangle[0] as usize],
            &mesh.vertices[triangle[1] as usize],
            &mesh.vertices[triangle[2] as usize],
        ];
        let uv = vertices
            .map(|vertex| DVec2::new(f64::from(vertex.uv[0]), f64::from(vertex.uv[1])) * scale);
        if uv.iter().any(|coordinate| !coordinate.is_finite()) {
            bail!("Mesh UV coordinate exceeds the supported bake range");
        }
        let area = edge(uv[0], uv[1], uv[2]);
        if !area.is_finite() || area.abs() < 1.0e-7 {
            continue;
        }
        let min_uv = uv[0].min(uv[1]).min(uv[2]);
        let max_uv = uv[0].max(uv[1]).max(uv[2]);
        if max_uv.x < 0.0 || max_uv.y < 0.0 || min_uv.x > scale || min_uv.y > scale {
            continue;
        }
        let min_x = min_uv.x.floor().max(0.0).min(scale) as u32;
        let min_y = min_uv.y.floor().max(0.0).min(scale) as u32;
        let max_x = max_uv.x.ceil().max(0.0).min(scale) as u32;
        let max_y = max_uv.y.ceil().max(0.0).min(scale) as u32;
        if min_x > max_x || min_y > max_y {
            continue;
        }
        let sample_count = u64::from(max_x - min_x + 1)
            .checked_mul(u64::from(max_y - min_y + 1))
            .context("Material-ID raster work estimate overflow")?;
        raster_samples = raster_samples.saturating_add(sample_count);
        if raster_samples > MAX_RASTER_SAMPLES {
            bail!("Material-ID bake exceeds the CPU rasterization safety budget");
        }
        let id = [slot as u8, (slot >> 8) as u8, (slot >> 16) as u8, 255];
        for y in min_y..=max_y {
            if cancel.load(Ordering::Relaxed) {
                bail!("Material-ID bake canceled");
            }
            for x in min_x..=max_x {
                let pixel = DVec2::new(f64::from(x), f64::from(y));
                let b0 = edge(uv[1], uv[2], pixel) / area;
                let b1 = edge(uv[2], uv[0], pixel) / area;
                let b2 = 1.0 - b0 - b1;
                if !b0.is_finite()
                    || !b1.is_finite()
                    || !b2.is_finite()
                    || b0 < -1.0e-5
                    || b1 < -1.0e-5
                    || b2 < -1.0e-5
                {
                    continue;
                }
                let offset = ((y as usize * size as usize) + x as usize) * 4;
                // Alpha doubles as the ownership mask, preserving the first
                // triangle's ID when UV islands overlap.
                if pixels[offset + 3] == 0 {
                    pixels[offset..offset + 4].copy_from_slice(&id);
                }
            }
        }
    }

    RgbaImage::from_raw(size, size, pixels).context("Could not construct material-ID bake image")
}

/// Bake high-mesh world normals onto the low mesh's UV atlas. Rays are cast
/// along both directions of each low-mesh interpolated normal and limited to
/// `max_ray_distance`; the nearest hit wins. Source material slots are not
/// filtered. The low mesh's Texture Set bindings filter destination texels.
pub fn bake_high_to_low_world_normals(
    low_mesh: &Mesh,
    high_mesh: &Mesh,
    options: HighToLowBakeOptions<'_>,
    cancel: &AtomicBool,
) -> Result<BakedMeshMaps> {
    low_mesh.validate()?;
    high_mesh.validate()?;
    if !options.max_ray_distance.is_finite() || options.max_ray_distance <= 0.0 {
        bail!("Maximum bake ray distance must be finite and greater than zero");
    }
    let texels = u64::from(options.size)
        .checked_mul(u64::from(options.size))
        .context("Mesh-map bake dimensions overflow")?;
    let output_bytes = texels
        .checked_mul(29)
        .context("Mesh-map bake working size overflow")?;
    let triangle_count = high_mesh.indices.len() / 3;
    let node_capacity = triangle_count
        .checked_mul(2)
        .and_then(|count| count.checked_sub(1))
        .context("Bake acceleration structure size overflow")?;
    let bvh_bytes = (triangle_count as u64)
        .checked_mul(std::mem::size_of::<SourceTriangle>() as u64)
        .and_then(|bytes| {
            bytes.checked_add(
                (triangle_count as u64).checked_mul(std::mem::size_of::<usize>() as u64)?,
            )
        })
        .and_then(|bytes| {
            bytes.checked_add(
                (node_capacity as u64).checked_mul(std::mem::size_of::<BvhNode>() as u64)?,
            )
        })
        .and_then(|bytes| bytes.checked_add(64 * std::mem::size_of::<usize>() as u64))
        .context("Bake acceleration structure size overflow")?;
    if output_bytes
        .checked_add(bvh_bytes)
        .is_none_or(|required| required > options.working_budget_bytes)
    {
        bail!("High-to-low bake exceeds the caller's transient-memory budget");
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("Mesh-map bake canceled");
    }

    let mut maps = bake_mesh_maps_budgeted(
        low_mesh,
        options.size,
        options.bindings,
        options.active_set_id,
        options.working_budget_bytes.saturating_sub(bvh_bytes),
        cancel,
    )?;
    let bvh = TriangleBvh::build(high_mesh, cancel)?;
    let mut stack = Vec::new();
    stack
        .try_reserve(64)
        .context("Could not reserve bake ray traversal stack")?;
    let max_distance = f64::from(options.max_ray_distance);
    for y in 0..options.size {
        if cancel.load(Ordering::Relaxed) {
            bail!("Mesh-map bake canceled");
        }
        for x in 0..options.size {
            let index = y as usize * options.size as usize + x as usize;
            if maps.valid[index] == 0 {
                continue;
            }
            let origin = DVec3::from_array(maps.positions[index].map(f64::from));
            let low_normal = DVec3::from_array(maps.world_normals[index].map(f64::from));
            let Some(low_normal) = low_normal.try_normalize() else {
                maps.valid[index] = 0;
                continue;
            };
            let positive = bvh.nearest_hit(
                high_mesh,
                origin + low_normal * RAY_ORIGIN_BIAS,
                low_normal,
                max_distance,
                &mut stack,
            );
            let negative = bvh.nearest_hit(
                high_mesh,
                origin - low_normal * RAY_ORIGIN_BIAS,
                -low_normal,
                max_distance,
                &mut stack,
            );
            let hit = match (positive, negative) {
                (Some(a), Some(b)) if b.distance < a.distance => Some(b),
                (Some(a), _) => Some(a),
                (None, b) => b,
            };
            if let Some(hit) = hit {
                maps.world_normals[index] = hit.world_normal.as_vec3().to_array();
            } else {
                maps.valid[index] = 0;
                maps.positions[index] = [0.0; 3];
                maps.world_normals[index] = [0.0; 3];
            }
        }
    }
    Ok(maps)
}

/// Bake deterministic, cosine-weighted self-occlusion into the active
/// Texture Set's UV atlas. Rays originate just above the rasterized surface
/// and travel into the hemisphere of its interpolated world normal. The value
/// is the fraction of rays that hit any triangle in the mesh within the
/// distance limit. Material slots filter destination texels only; all mesh
/// triangles can occlude. The editor may run this primitive on a worker and
/// install the encoded output as an editable layer; the per-set settings are
/// stored as project metadata by the editor.
pub fn bake_ambient_occlusion(
    mesh: &Mesh,
    options: AmbientOcclusionBakeOptions<'_>,
    cancel: &AtomicBool,
) -> Result<BakedAmbientOcclusion> {
    bake_ambient_occlusion_with_progress(mesh, options, cancel, |_| {})
}

fn bake_ambient_occlusion_with_progress(
    mesh: &Mesh,
    options: AmbientOcclusionBakeOptions<'_>,
    cancel: &AtomicBool,
    mut report_completed_rows: impl FnMut(u32),
) -> Result<BakedAmbientOcclusion> {
    mesh.validate()?;
    if options.sample_count == 0 || options.sample_count > MAX_AO_SAMPLES_PER_TEXEL {
        bail!("Ambient occlusion sample count must be between 1 and {MAX_AO_SAMPLES_PER_TEXEL}");
    }
    if !options.max_ray_distance.is_finite() || options.max_ray_distance <= 0.0 {
        bail!("Ambient occlusion ray distance must be finite and greater than zero");
    }

    let texels = u64::from(options.size)
        .checked_mul(u64::from(options.size))
        .context("Ambient occlusion dimensions overflow")?;
    let output_and_maps_bytes = texels
        .checked_mul(34) // AO f32 + validity byte, plus temporary mesh maps (29 bytes).
        .context("Ambient occlusion working size overflow")?;
    let triangle_count = mesh.indices.len() / 3;
    let node_capacity = triangle_count
        .checked_mul(2)
        .and_then(|count| count.checked_sub(1))
        .context("Ambient occlusion BVH node count overflow")?;
    let bvh_bytes = (triangle_count as u64)
        .checked_mul(std::mem::size_of::<SourceTriangle>() as u64)
        .and_then(|bytes| {
            bytes.checked_add(
                (triangle_count as u64).checked_mul(std::mem::size_of::<usize>() as u64)?,
            )
        })
        .and_then(|bytes| {
            bytes.checked_add(
                (node_capacity as u64).checked_mul(std::mem::size_of::<BvhNode>() as u64)?,
            )
        })
        .and_then(|bytes| bytes.checked_add(64 * std::mem::size_of::<usize>() as u64))
        .context("Ambient occlusion BVH size overflow")?;
    if output_and_maps_bytes
        .checked_add(bvh_bytes)
        .is_none_or(|required| required > options.working_budget_bytes)
    {
        bail!("Ambient occlusion bake exceeds the caller's transient-memory budget");
    }
    if texels
        .checked_mul(u64::from(options.sample_count))
        .is_none_or(|rays| rays > MAX_AO_RAYS)
    {
        bail!("Ambient occlusion bake exceeds the CPU ray-count safety budget");
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("Ambient occlusion bake canceled");
    }

    let maps = bake_mesh_maps_budgeted(
        mesh,
        options.size,
        options.bindings,
        options.active_set_id,
        options.working_budget_bytes.saturating_sub(bvh_bytes),
        cancel,
    )?;
    let bvh = TriangleBvh::build(mesh, cancel)?;
    let mut stack = Vec::new();
    stack
        .try_reserve(64)
        .context("Could not reserve ambient occlusion ray traversal stack")?;
    let texel_count = usize::try_from(texels).context("Ambient occlusion texel count overflow")?;
    let mut occlusion = reserve_filled(texel_count, 0.0_f32)?;
    let max_distance = f64::from(options.max_ray_distance);
    let mut rays_since_cancel_check = 0u32;

    for y in 0..options.size {
        if cancel.load(Ordering::Relaxed) {
            bail!("Ambient occlusion bake canceled");
        }
        for x in 0..options.size {
            let index = y as usize * options.size as usize + x as usize;
            if maps.valid[index] == 0 {
                continue;
            }
            let origin = DVec3::from_array(maps.positions[index].map(f64::from));
            let Some(normal) =
                DVec3::from_array(maps.world_normals[index].map(f64::from)).try_normalize()
            else {
                continue;
            };
            let (tangent, bitangent) = tangent_frame(normal);
            let mut random = SplitMix64::new(texel_seed(options.seed, index as u64));
            let mut hits = 0u32;
            for _ in 0..options.sample_count {
                if rays_since_cancel_check == 0 {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("Ambient occlusion bake canceled");
                    }
                    rays_since_cancel_check = 256;
                }
                rays_since_cancel_check -= 1;
                let u1 = random.unit_f64();
                let u2 = random.unit_f64();
                let radius = u1.sqrt();
                let angle = std::f64::consts::TAU * u2;
                let local_x = radius * angle.cos();
                let local_y = radius * angle.sin();
                let local_z = (1.0 - u1).sqrt();
                let direction = (tangent * local_x + bitangent * local_y + normal * local_z)
                    .try_normalize()
                    .unwrap_or(normal);
                if bvh
                    .nearest_hit(
                        mesh,
                        origin + normal * RAY_ORIGIN_BIAS,
                        direction,
                        max_distance,
                        &mut stack,
                    )
                    .is_some()
                {
                    hits += 1;
                }
            }
            occlusion[index] = hits as f32 / options.sample_count as f32;
        }
        report_completed_rows(y + 1);
    }

    Ok(BakedAmbientOcclusion {
        size: options.size,
        occlusion,
        valid: maps.valid,
    })
}

// Curvature neighborhood estimator and thickness convention adapted from
// SubCraft 71e49fde7e4e1dba188a6589c9e5a139d1e3d9d5, crates/bake/src/{curvature,lib}.rs.
// Copyright (c) Aalzard and the SubCraft contributors. Used under MIT;
// see THIRD_PARTY_NOTICES.md. Traversal, bounds, cancellation and image output
// use Kalium's existing baker. No bundled SubCraft artwork is required.

/// Bake seam-independent 3D-neighborhood curvature: black is concave,
/// middle gray is flat, white is convex. Uncovered texels are transparent.
pub fn bake_curvature(
    mesh: &Mesh,
    options: SurfaceMapBakeOptions<'_>,
    cancel: &AtomicBool,
) -> Result<RgbaImage> {
    bake_surface_map(mesh, options, cancel, true)
}

/// Bake cosine-weighted inward ray thickness. White means thin, black means
/// no opposite surface within `max_ray_distance`. This is a local geometric
/// estimate, not a watertight-solid volume measurement.
pub fn bake_thickness(
    mesh: &Mesh,
    options: SurfaceMapBakeOptions<'_>,
    cancel: &AtomicBool,
) -> Result<RgbaImage> {
    bake_surface_map(mesh, options, cancel, false)
}

fn bake_surface_map(
    mesh: &Mesh,
    options: SurfaceMapBakeOptions<'_>,
    cancel: &AtomicBool,
    curvature: bool,
) -> Result<RgbaImage> {
    mesh.validate()?;
    if !(64..=MAX_BAKE_SIZE).contains(&options.size) {
        bail!("Surface-map dimension must be between 64 and {MAX_BAKE_SIZE}");
    }
    if curvature {
        if !options.curvature_radius.is_finite() || options.curvature_radius <= 0.0 {
            bail!("Curvature radius must be finite and positive");
        }
    } else if !options.max_ray_distance.is_finite() || options.max_ray_distance <= 0.0 {
        bail!("Thickness ray distance must be finite and positive");
    }
    let texels = u64::from(options.size) * u64::from(options.size);
    if !curvature
        && (options.sample_count == 0
            || options.sample_count > MAX_AO_SAMPLES_PER_TEXEL
            || texels * u64::from(options.sample_count) > MAX_AO_RAYS)
    {
        bail!("Thickness sample count exceeds the CPU ray-count budget");
    }
    let triangles = mesh.indices.len() / 3;
    let nodes = triangles
        .checked_mul(2)
        .and_then(|v| v.checked_sub(1))
        .context("Surface-map BVH node count overflow")?;
    let bvh_bytes = (triangles as u64)
        .checked_mul((std::mem::size_of::<SourceTriangle>() + std::mem::size_of::<usize>()) as u64)
        .and_then(|v| {
            v.checked_add((nodes as u64).checked_mul(std::mem::size_of::<BvhNode>() as u64)?)
        })
        .and_then(|v| v.checked_add(64 * std::mem::size_of::<usize>() as u64))
        .context("Surface-map memory budget overflow")?;
    if (texels * 33)
        .checked_add(bvh_bytes)
        .is_none_or(|v| v > options.working_budget_bytes)
    {
        bail!("Surface-map bake exceeds transient-memory budget");
    }
    let maps = bake_mesh_maps_budgeted(
        mesh,
        options.size,
        options.bindings,
        options.active_set_id,
        options.working_budget_bytes - bvh_bytes - texels * 4,
        cancel,
    )?;
    let bvh = TriangleBvh::build(mesh, cancel)?;
    let mut stack = Vec::new();
    stack
        .try_reserve_exact(64)
        .context("Could not reserve surface-map traversal stack")?;
    let pixels = reserve_filled((texels * 4) as usize, 0_u8)?;
    let mut image = RgbaImage::from_raw(options.size, options.size, pixels)
        .context("Could not allocate surface-map image")?;
    // Global traversal cap additionally bounds dense curvature neighborhoods.
    let mut visits = 0_u64;
    for (index, pixel) in image.pixels_mut().enumerate() {
        if index.is_multiple_of(64) && cancel.load(Ordering::Relaxed) {
            bail!("Surface-map bake canceled");
        }
        if maps.valid[index] == 0 {
            continue;
        }
        let p = DVec3::from_array(maps.positions[index].map(f64::from));
        let Some(n) = DVec3::from_array(maps.world_normals[index].map(f64::from)).try_normalize()
        else {
            continue;
        };
        let value = if curvature {
            neighborhood_curvature(
                mesh,
                &bvh,
                p,
                n,
                f64::from(options.curvature_radius),
                &mut stack,
                &mut visits,
                cancel,
            )?
        } else {
            let (tangent, bitangent) = tangent_frame(-n);
            let mut random = SplitMix64::new(texel_seed(options.seed, index as u64));
            let maximum = f64::from(options.max_ray_distance);
            let mut total = 0.0;
            for sample in 0..options.sample_count {
                if sample.is_multiple_of(64) && cancel.load(Ordering::Relaxed) {
                    bail!("Thickness bake canceled");
                }
                let radius = random.unit_f64().sqrt();
                let angle = std::f64::consts::TAU * random.unit_f64();
                let direction = tangent * (radius * angle.cos())
                    + bitangent * (radius * angle.sin())
                    - n * (1.0 - radius * radius).sqrt();
                total += bvh
                    .nearest_hit(
                        mesh,
                        p - n * RAY_ORIGIN_BIAS,
                        direction,
                        maximum,
                        &mut stack,
                    )
                    .map_or(maximum, |hit| (hit.distance + RAY_ORIGIN_BIAS).min(maximum));
            }
            1.0 - total / (f64::from(options.sample_count) * maximum)
        };
        let encoded = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        *pixel = Rgba([encoded, encoded, encoded, 255]);
    }
    Ok(image)
}

#[allow(clippy::too_many_arguments)]
fn neighborhood_curvature(
    mesh: &Mesh,
    bvh: &TriangleBvh,
    p: DVec3,
    n: DVec3,
    radius: f64,
    stack: &mut Vec<usize>,
    visits: &mut u64,
    cancel: &AtomicBool,
) -> Result<f64> {
    stack.clear();
    stack.push(0);
    let (mut sum, mut weight, mut nearest) = (0.0, 0.0, 0.0_f64);
    while let Some(node_index) = stack.pop() {
        *visits += 1;
        if *visits > MAX_AO_RAYS {
            bail!("Curvature neighborhood exceeds CPU traversal budget");
        }
        if visits.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
            bail!("Curvature bake canceled");
        }
        let node = &bvh.nodes[node_index];
        let closest = p.clamp(node.bounds.min, node.bounds.max);
        if closest.distance_squared(p) >= radius * radius {
            continue;
        }
        match node.kind {
            NodeKind::Branch { left, right } => {
                stack.push(right);
                stack.push(left);
            }
            NodeKind::Leaf { start, end } => {
                for order in start..end {
                    *visits += 1;
                    if *visits > MAX_AO_RAYS {
                        bail!("Curvature neighborhood exceeds CPU traversal budget");
                    }
                    let tri = &bvh.triangles[bvh.order[order]];
                    let vertices = tri.indices.map(|i| &mesh.vertices[i as usize]);
                    let [a, b, c] = vertices.map(|v| DVec3::from_array(v.position.map(f64::from)));
                    // Degenerate faces cannot contribute a meaningful closest point.
                    if (b - a).cross(c - a).length_squared() <= 1.0e-28 {
                        continue;
                    }
                    let (q, bary) = closest_triangle_point(p, a, b, c);
                    let mut d = q - p;
                    let mut distance = d.length();
                    if distance >= radius {
                        continue;
                    }
                    if distance <= radius * 1.0e-3 {
                        let Some(direction) = ((a + b + c) / 3.0 - p).try_normalize() else {
                            continue;
                        };
                        distance = radius * 1.0e-3;
                        d = direction * distance;
                    }
                    let normal = DVec3::from_array(vertices[0].normal.map(f64::from)) * bary[0]
                        + DVec3::from_array(vertices[1].normal.map(f64::from)) * bary[1]
                        + DVec3::from_array(vertices[2].normal.map(f64::from)) * bary[2];
                    let Some(normal) = normal.try_normalize() else {
                        continue;
                    };
                    let difference = normal - n;
                    if difference.length_squared() < 1.0e-6 {
                        continue;
                    }
                    let w = 1.0 - distance / radius;
                    sum += w * difference.dot(d / distance);
                    weight += w;
                    nearest = nearest.max(w);
                }
            }
        }
    }
    Ok(if weight > 0.0 {
        0.5 + 0.5 * (sum / weight).clamp(-1.0, 1.0) * nearest
    } else {
        0.5
    })
}

// Closest point and barycentric coordinates, following the SubCraft MIT
// implementation of Ericson's triangle Voronoi-region construction.
fn closest_triangle_point(p: DVec3, a: DVec3, b: DVec3, c: DVec3) -> (DVec3, [f64; 3]) {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return (a, [1.0, 0.0, 0.0]);
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return (b, [0.0, 1.0, 0.0]);
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return (a + ab * v, [1.0 - v, v, 0.0]);
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return (c, [0.0, 0.0, 1.0]);
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return (a + ac * w, [1.0 - w, 0.0, w]);
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && d4 - d3 >= 0.0 && d5 - d6 >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return (b + (c - b) * w, [0.0, 1.0 - w, w]);
    }
    let denom = (va + vb + vc).recip();
    let v = vb * denom;
    let w = vc * denom;
    (a + ab * v + ac * w, [1.0 - v - w, v, w])
}

fn tangent_frame(normal: DVec3) -> (DVec3, DVec3) {
    let reference = if normal.z.abs() < 0.999 {
        DVec3::Z
    } else {
        DVec3::Y
    };
    let tangent = reference.cross(normal).normalize();
    (tangent, normal.cross(tangent).normalize())
}

fn texel_seed(seed: u64, texel_index: u64) -> u64 {
    seed ^ texel_index.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn unit_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
    }
}

#[derive(Clone, Copy)]
struct Bounds {
    min: DVec3,
    max: DVec3,
}

impl Bounds {
    fn triangle(points: [DVec3; 3]) -> Self {
        Self {
            min: points[0].min(points[1]).min(points[2]),
            max: points[0].max(points[1]).max(points[2]),
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    fn ray_entry(self, origin: DVec3, direction: DVec3, max_distance: f64) -> Option<f64> {
        let mut near = 0.0_f64;
        let mut far = max_distance;
        for axis in 0..3 {
            let o = origin[axis];
            let d = direction[axis];
            if d.abs() <= 1.0e-15 {
                if o < self.min[axis] || o > self.max[axis] {
                    return None;
                }
                continue;
            }
            let inverse = d.recip();
            let mut first = (self.min[axis] - o) * inverse;
            let mut second = (self.max[axis] - o) * inverse;
            if first > second {
                std::mem::swap(&mut first, &mut second);
            }
            near = near.max(first);
            far = far.min(second);
            if near > far {
                return None;
            }
        }
        (far >= 0.0 && near <= max_distance).then_some(near.max(0.0))
    }
}

struct SourceTriangle {
    indices: [u32; 3],
    bounds: Bounds,
    centroid: DVec3,
}

enum NodeKind {
    Leaf { start: usize, end: usize },
    Branch { left: usize, right: usize },
}

struct BvhNode {
    bounds: Bounds,
    kind: NodeKind,
}

struct TriangleBvh {
    triangles: Vec<SourceTriangle>,
    order: Vec<usize>,
    nodes: Vec<BvhNode>,
}

#[derive(Clone, Copy)]
struct RayHit {
    distance: f64,
    triangle_index: usize,
    world_normal: DVec3,
}

impl TriangleBvh {
    fn build(mesh: &Mesh, cancel: &AtomicBool) -> Result<Self> {
        let triangle_count = mesh.indices.len() / 3;
        let mut triangles = Vec::new();
        triangles
            .try_reserve_exact(triangle_count)
            .context("Could not reserve bake source triangles")?;
        for (triangle_index, indices) in mesh.indices.chunks_exact(3).enumerate() {
            if triangle_index.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
                bail!("Mesh-map bake canceled");
            }
            let indices = [indices[0], indices[1], indices[2]];
            let points = indices.map(|index| {
                DVec3::from_array(mesh.vertices[index as usize].position.map(f64::from))
            });
            let bounds = Bounds::triangle(points);
            triangles.push(SourceTriangle {
                indices,
                bounds,
                centroid: (points[0] + points[1] + points[2]) / 3.0,
            });
        }
        let order: Vec<usize> = (0..triangle_count).collect();
        let mut nodes = Vec::new();
        let node_capacity = triangle_count
            .checked_mul(2)
            .and_then(|count| count.checked_sub(1))
            .context("Bake BVH node count overflow")?;
        nodes
            .try_reserve_exact(node_capacity)
            .context("Could not reserve bake BVH nodes")?;
        let mut bvh = Self {
            triangles,
            order,
            nodes,
        };
        let order_len = bvh.order.len();
        bvh.build_node(0, order_len, cancel)?;
        Ok(bvh)
    }

    fn build_node(&mut self, start: usize, end: usize, cancel: &AtomicBool) -> Result<usize> {
        if cancel.load(Ordering::Relaxed) {
            bail!("Mesh-map bake canceled");
        }
        let mut bounds = self.triangles[self.order[start]].bounds;
        let mut centroid_min = self.triangles[self.order[start]].centroid;
        let mut centroid_max = centroid_min;
        for position in start + 1..end {
            let triangle = &self.triangles[self.order[position]];
            bounds = bounds.union(triangle.bounds);
            centroid_min = centroid_min.min(triangle.centroid);
            centroid_max = centroid_max.max(triangle.centroid);
        }
        let node_index = self.nodes.len();
        self.nodes.push(BvhNode {
            bounds,
            kind: NodeKind::Leaf { start, end },
        });
        if end - start <= BVH_NODE_LEAF_TRIANGLES {
            return Ok(node_index);
        }
        let extent = centroid_max - centroid_min;
        let axis = if extent.x >= extent.y && extent.x >= extent.z {
            0
        } else if extent.y >= extent.z {
            1
        } else {
            2
        };
        self.order[start..end].sort_unstable_by(|left, right| {
            self.triangles[*left].centroid[axis]
                .total_cmp(&self.triangles[*right].centroid[axis])
                .then_with(|| left.cmp(right))
        });
        let middle = start + (end - start) / 2;
        let left = self.build_node(start, middle, cancel)?;
        let right = self.build_node(middle, end, cancel)?;
        self.nodes[node_index].kind = NodeKind::Branch { left, right };
        Ok(node_index)
    }

    fn nearest_hit(
        &self,
        mesh: &Mesh,
        origin: DVec3,
        direction: DVec3,
        max_distance: f64,
        stack: &mut Vec<usize>,
    ) -> Option<RayHit> {
        stack.clear();
        stack.push(0);
        let mut nearest: Option<RayHit> = None;
        while let Some(node_index) = stack.pop() {
            let current_limit = nearest.map_or(max_distance, |hit| hit.distance);
            let node = &self.nodes[node_index];
            if node
                .bounds
                .ray_entry(origin, direction, current_limit)
                .is_none()
            {
                continue;
            }
            match node.kind {
                NodeKind::Leaf { start, end } => {
                    for order_index in start..end {
                        let triangle_index = self.order[order_index];
                        let triangle = &self.triangles[triangle_index];
                        let candidate = ray_triangle_hit(
                            mesh,
                            triangle_index,
                            triangle,
                            origin,
                            direction,
                            current_limit,
                        );
                        if let Some(candidate) = candidate {
                            let replace = nearest.is_none_or(|current| {
                                candidate.distance < current.distance - 1.0e-10
                                    || ((candidate.distance - current.distance).abs() <= 1.0e-10
                                        && candidate.triangle_index < current.triangle_index)
                            });
                            if replace {
                                nearest = Some(candidate);
                            }
                        }
                    }
                }
                NodeKind::Branch { left, right } => {
                    let left_entry =
                        self.nodes[left]
                            .bounds
                            .ray_entry(origin, direction, current_limit);
                    let right_entry =
                        self.nodes[right]
                            .bounds
                            .ray_entry(origin, direction, current_limit);
                    match (left_entry, right_entry) {
                        (Some(a), Some(b)) if a <= b => {
                            stack.push(right);
                            stack.push(left);
                        }
                        (Some(_), Some(_)) => {
                            stack.push(left);
                            stack.push(right);
                        }
                        (Some(_), None) => stack.push(left),
                        (None, Some(_)) => stack.push(right),
                        (None, None) => {}
                    }
                }
            }
        }
        nearest
    }
}

fn ray_triangle_hit(
    mesh: &Mesh,
    triangle_index: usize,
    triangle: &SourceTriangle,
    origin: DVec3,
    direction: DVec3,
    max_distance: f64,
) -> Option<RayHit> {
    let points = triangle
        .indices
        .map(|index| DVec3::from_array(mesh.vertices[index as usize].position.map(f64::from)));
    let edge1 = points[1] - points[0];
    let edge2 = points[2] - points[0];
    let p = direction.cross(edge2);
    let determinant = edge1.dot(p);
    if !determinant.is_finite() || determinant.abs() < 1.0e-14 {
        return None;
    }
    let inverse = determinant.recip();
    let from_a = origin - points[0];
    let b1 = from_a.dot(p) * inverse;
    if !(-1.0e-10..=1.0 + 1.0e-10).contains(&b1) {
        return None;
    }
    let q = from_a.cross(edge1);
    let b2 = direction.dot(q) * inverse;
    if b2 < -1.0e-10 || b1 + b2 > 1.0 + 1.0e-10 {
        return None;
    }
    let distance = edge2.dot(q) * inverse;
    if !distance.is_finite() || distance <= RAY_ORIGIN_BIAS || distance > max_distance {
        return None;
    }
    let b0 = 1.0 - b1 - b2;
    let vertices = triangle.indices.map(|index| &mesh.vertices[index as usize]);
    let interpolated = DVec3::from_array(vertices[0].normal.map(f64::from)) * b0
        + DVec3::from_array(vertices[1].normal.map(f64::from)) * b1
        + DVec3::from_array(vertices[2].normal.map(f64::from)) * b2;
    let geometric = (points[1] - points[0])
        .cross(points[2] - points[0])
        .try_normalize()?;
    let world_normal = interpolated.try_normalize().unwrap_or(geometric);
    Some(RayHit {
        distance,
        triangle_index,
        world_normal,
    })
}

fn bake_mesh_maps_with_cancel(
    mesh: &Mesh,
    size: u32,
    bindings: &[Option<u32>],
    active_set_id: u32,
    working_budget_bytes: u64,
    mut is_canceled: impl FnMut() -> bool,
) -> Result<BakedMeshMaps> {
    mesh.validate()?;
    if !(64..=MAX_BAKE_SIZE).contains(&size) {
        bail!("Mesh-map bake dimension must be between 64 and {MAX_BAKE_SIZE}");
    }
    if active_set_id == 0 {
        bail!("Active Texture Set ID must be nonzero");
    }
    if bindings.len() != mesh.material_count() {
        bail!("Material binding count does not match mesh slots");
    }
    if bindings.iter().flatten().any(|id| *id == 0) {
        bail!("Texture Set binding IDs must be nonzero");
    }
    if is_canceled() {
        bail!("Mesh-map bake canceled");
    }

    let texel_count = u64::from(size)
        .checked_mul(u64::from(size))
        .context("Mesh-map bake dimensions overflow")?;
    // Two 12-byte float maps, one validity byte, and one temporary RGBA
    // encoding buffer used when writing a result.
    let working_bytes = texel_count
        .checked_mul(29)
        .context("Mesh-map bake working size overflow")?;
    if working_bytes > working_budget_bytes {
        bail!("Mesh-map bake exceeds the caller's transient-memory budget");
    }
    let texel_count = usize::try_from(texel_count).context("Mesh-map texel count overflow")?;
    let mut positions = reserve_filled(texel_count, [0.0; 3])?;
    let mut world_normals = reserve_filled(texel_count, [0.0; 3])?;
    let mut valid = reserve_filled(texel_count, 0u8)?;
    let scale = f64::from(size - 1);
    let (position_min, position_max) = mesh.vertices.iter().fold(
        (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
        |(minimum, maximum), vertex| {
            let position = Vec3::from_array(vertex.position);
            (minimum.min(position), maximum.max(position))
        },
    );
    let mut raster_samples = 0u64;

    for (triangle_index, triangle) in mesh.indices.chunks_exact(3).enumerate() {
        if is_canceled() {
            bail!("Mesh-map bake canceled");
        }
        let slot = mesh.triangle_material(triangle_index);
        let assigned_set = bindings
            .get(slot)
            .copied()
            .flatten()
            .unwrap_or(active_set_id);
        if assigned_set != active_set_id {
            continue;
        }

        let vertices = [
            &mesh.vertices[triangle[0] as usize],
            &mesh.vertices[triangle[1] as usize],
            &mesh.vertices[triangle[2] as usize],
        ];
        let uv = vertices
            .map(|vertex| DVec2::new(f64::from(vertex.uv[0]), f64::from(vertex.uv[1])) * scale);
        if uv.iter().any(|coordinate| !coordinate.is_finite()) {
            bail!("Mesh UV coordinate exceeds the supported bake range");
        }
        let area = edge(uv[0], uv[1], uv[2]);
        if !area.is_finite() || area.abs() < 1.0e-7 {
            continue;
        }
        let min_uv = uv[0].min(uv[1]).min(uv[2]);
        let max_uv = uv[0].max(uv[1]).max(uv[2]);
        if max_uv.x < 0.0 || max_uv.y < 0.0 || min_uv.x > scale || min_uv.y > scale {
            continue;
        }
        let min_x = min_uv.x.floor().max(0.0).min(scale) as u32;
        let min_y = min_uv.y.floor().max(0.0).min(scale) as u32;
        let max_x = max_uv.x.ceil().max(0.0).min(scale) as u32;
        let max_y = max_uv.y.ceil().max(0.0).min(scale) as u32;
        if min_x > max_x || min_y > max_y {
            continue;
        }
        let sample_count = u64::from(max_x - min_x + 1)
            .checked_mul(u64::from(max_y - min_y + 1))
            .context("Mesh-map raster work estimate overflow")?;
        raster_samples = raster_samples.saturating_add(sample_count);
        if raster_samples > MAX_RASTER_SAMPLES {
            bail!("Mesh-map bake exceeds the CPU rasterization safety budget");
        }

        let positions_tri =
            vertices.map(|vertex| DVec3::from_array(vertex.position.map(f64::from)));
        let normals_tri = vertices.map(|vertex| DVec3::from_array(vertex.normal.map(f64::from)));
        let geometric_normal = (positions_tri[1] - positions_tri[0])
            .cross(positions_tri[2] - positions_tri[0])
            .try_normalize();
        let Some(geometric_normal) = geometric_normal else {
            continue;
        };
        for y in min_y..=max_y {
            if is_canceled() {
                bail!("Mesh-map bake canceled");
            }
            for x in min_x..=max_x {
                let pixel = DVec2::new(f64::from(x), f64::from(y));
                let b0 = edge(uv[1], uv[2], pixel) / area;
                let b1 = edge(uv[2], uv[0], pixel) / area;
                let b2 = 1.0 - b0 - b1;
                if !b0.is_finite()
                    || !b1.is_finite()
                    || !b2.is_finite()
                    || b0 < -1.0e-5
                    || b1 < -1.0e-5
                    || b2 < -1.0e-5
                {
                    continue;
                }
                let index = (y as usize) * size as usize + x as usize;
                if valid[index] != 0 {
                    continue;
                }
                let position =
                    positions_tri[0] * b0 + positions_tri[1] * b1 + positions_tri[2] * b2;
                let normal = (normals_tri[0] * b0 + normals_tri[1] * b1 + normals_tri[2] * b2)
                    .try_normalize()
                    .unwrap_or(geometric_normal);
                positions[index] = position.as_vec3().to_array();
                world_normals[index] = normal.as_vec3().to_array();
                valid[index] = 255;
            }
        }
    }

    Ok(BakedMeshMaps {
        size,
        positions,
        world_normals,
        valid,
        position_min: position_min.to_array(),
        position_max: position_max.to_array(),
    })
}

fn reserve_filled<T: Clone>(count: usize, value: T) -> Result<Vec<T>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(count)
        .context("Could not reserve mesh-map bake output memory")?;
    output.resize(count, value);
    Ok(output)
}

fn edge(a: DVec2, b: DVec2, point: DVec2) -> f64 {
    (point.x - a.x) * (b.y - a.y) - (point.y - a.y) * (b.x - a.x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::Vertex;
    use serde_json::Map;
    use std::{
        sync::atomic::AtomicBool,
        thread,
        time::{Duration, Instant},
    };

    fn surface_options() -> SurfaceMapBakeOptions<'static> {
        SurfaceMapBakeOptions {
            size: 64,
            bindings: &[Some(1)],
            active_set_id: 1,
            max_ray_distance: 1.0,
            curvature_radius: 0.4,
            sample_count: 16,
            seed: 73,
            working_budget_bytes: 128 * 1024 * 1024,
        }
    }

    #[test]
    fn subcraft_curvature_flat_and_thickness_open_are_neutral() {
        let mesh = triangle_mesh();
        let cancel = AtomicBool::new(false);
        let curvature = bake_curvature(&mesh, surface_options(), &cancel).unwrap();
        let thickness = bake_thickness(&mesh, surface_options(), &cancel).unwrap();
        assert!(curvature.pixels().any(|p| p[3] == 255));
        for (c, t) in curvature.pixels().zip(thickness.pixels()) {
            if c[3] != 0 {
                assert_eq!(c.0, [128, 128, 128, 255]);
                assert_eq!(t.0, [0, 0, 0, 255]);
            } else {
                assert_eq!(t[3], 0);
            }
        }
        assert_eq!(
            curvature.as_raw(),
            bake_curvature(&mesh, surface_options(), &cancel)
                .unwrap()
                .as_raw()
        );
    }

    #[test]
    fn subcraft_curvature_distinguishes_convex_and_concave_across_uv_seam() {
        let cancel = AtomicBool::new(false);
        let corner = |concave: bool| {
            let mut mesh = triangle_mesh();
            // Separate vertices and a distant UV island deliberately retain the seam.
            let z = if concave { 1.0 } else { -1.0 };
            let normal = if concave {
                [1.0, 0.0, 0.0]
            } else {
                [-1.0, 0.0, 0.0]
            };
            let first = mesh.vertices.len() as u32;
            mesh.vertices.extend([
                vertex([0.0, 0.0, 0.0], normal, [0.95, 0.95]),
                vertex([0.0, 1.0, 0.0], normal, [0.99, 0.95]),
                vertex([0.0, 0.0, z], normal, [0.95, 0.99]),
            ]);
            mesh.indices.extend([first, first + 1, first + 2]);
            mesh.triangle_materials.push(0);
            mesh
        };
        let convex = bake_curvature(&corner(false), surface_options(), &cancel).unwrap();
        let concave = bake_curvature(&corner(true), surface_options(), &cancel).unwrap();
        // Near x=0, away from both the diagonal and separate island.
        assert!(convex.get_pixel(8, 40)[0] > 128);
        assert!(concave.get_pixel(8, 40)[0] < 128);
    }

    #[test]
    fn subcraft_thickness_detects_opposite_surface_and_is_deterministic() {
        let mut mesh = triangle_mesh();
        let first = mesh.vertices.len() as u32;
        mesh.vertices.extend([
            vertex([-10.0, -10.0, -0.1], [0.0, 0.0, -1.0], [0.95, 0.95]),
            vertex([20.0, -10.0, -0.1], [0.0, 0.0, -1.0], [0.99, 0.95]),
            vertex([-10.0, 20.0, -0.1], [0.0, 0.0, -1.0], [0.95, 0.99]),
        ]);
        mesh.indices.extend([first, first + 1, first + 2]);
        mesh.triangle_materials.push(0);
        let cancel = AtomicBool::new(false);
        let result = bake_thickness(&mesh, surface_options(), &cancel).unwrap();
        assert!(result.get_pixel(16, 40)[0] > 128);
        assert_eq!(
            result.as_raw(),
            bake_thickness(&mesh, surface_options(), &cancel)
                .unwrap()
                .as_raw()
        );
    }

    #[test]
    fn subcraft_surface_maps_enforce_cancel_memory_bindings_and_numeric_limits() {
        let mesh = triangle_mesh();
        let cancel = AtomicBool::new(false);
        for curvature in [true, false] {
            let bake = |o, c: &AtomicBool| bake_surface_map(&mesh, o, c, curvature);
            assert!(bake(surface_options(), &AtomicBool::new(true)).is_err());
            assert!(bake(
                SurfaceMapBakeOptions {
                    working_budget_bytes: 1,
                    ..surface_options()
                },
                &cancel
            )
            .is_err());
            assert!(bake(
                SurfaceMapBakeOptions {
                    bindings: &[],
                    ..surface_options()
                },
                &cancel
            )
            .is_err());
            assert!(bake(
                SurfaceMapBakeOptions {
                    size: u32::MAX,
                    ..surface_options()
                },
                &cancel
            )
            .is_err());
        }
        assert!(bake_curvature(
            &mesh,
            SurfaceMapBakeOptions {
                curvature_radius: f32::NAN,
                ..surface_options()
            },
            &cancel
        )
        .is_err());
        assert!(bake_thickness(
            &mesh,
            SurfaceMapBakeOptions {
                sample_count: 0,
                ..surface_options()
            },
            &cancel
        )
        .is_err());
        assert!(bake_thickness(
            &mesh,
            SurfaceMapBakeOptions {
                max_ray_distance: f32::INFINITY,
                ..surface_options()
            },
            &cancel
        )
        .is_err());
        let blank = bake_curvature(
            &mesh,
            SurfaceMapBakeOptions {
                bindings: &[Some(2)],
                ..surface_options()
            },
            &cancel,
        )
        .unwrap();
        assert!(blank.pixels().all(|p| p[3] == 0));
    }

    fn triangle_mesh() -> Mesh {
        Mesh {
            name: "Bake triangle".into(),
            vertices: vec![
                vertex([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.1, 0.1]),
                vertex([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.9, 0.1]),
                vertex([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.1, 0.9]),
            ],
            indices: vec![0, 1, 2],
            materials: vec!["Default".into()],
            triangle_materials: vec![0],
            extra_uv_sets: Default::default(),
            extensions: Map::new(),
        }
    }

    fn offset_triangle(z: f32, normal: [f32; 3]) -> Mesh {
        Mesh {
            name: "Offset bake triangle".into(),
            vertices: vec![
                vertex([0.0, 0.0, z], normal, [0.1, 0.1]),
                vertex([1.0, 0.0, z], normal, [0.9, 0.1]),
                vertex([0.0, 1.0, z], normal, [0.1, 0.9]),
            ],
            indices: vec![0, 1, 2],
            materials: vec!["Default".into()],
            triangle_materials: vec![0],
            extra_uv_sets: Default::default(),
            extensions: Map::new(),
        }
    }

    fn vertex(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> Vertex {
        Vertex {
            position,
            normal,
            uv,
            tangent: None,
            extensions: Map::new(),
        }
    }

    fn bake(mesh: &Mesh, bindings: &[Option<u32>], set_id: u32) -> Result<BakedMeshMaps> {
        bake_mesh_maps_budgeted(
            mesh,
            64,
            bindings,
            set_id,
            128 * 1024 * 1024,
            &AtomicBool::new(false),
        )
    }

    fn two_slot_id_mesh() -> Mesh {
        Mesh {
            name: "Two-slot ID bake".into(),
            vertices: vec![
                vertex([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.1, 0.1]),
                vertex([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.45, 0.1]),
                vertex([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.1, 0.45]),
                vertex([2.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.55, 0.55]),
                vertex([3.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.9, 0.55]),
                vertex([2.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.55, 0.9]),
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
            materials: vec!["First".into(), "Second".into()],
            triangle_materials: vec![0, 1],
            extra_uv_sets: Default::default(),
            extensions: Map::new(),
        }
    }

    #[test]
    fn material_id_bake_encodes_slots_and_leaves_uv_gaps_transparent() {
        let mesh = two_slot_id_mesh();
        let image = bake_material_id(
            &mesh,
            64,
            &[None, None],
            1,
            64 * 64 * 4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(image.get_pixel(10, 10).0, [0, 0, 0, 255]);
        assert_eq!(image.get_pixel(40, 40).0, [1, 0, 0, 255]);
        assert_eq!(image.get_pixel(30, 30).0, [0, 0, 0, 0]);
    }

    #[test]
    fn material_id_bake_filters_by_texture_set_and_is_deterministic() {
        let mesh = two_slot_id_mesh();
        let all = bake_material_id(
            &mesh,
            64,
            &[None, None],
            1,
            64 * 64 * 4,
            &AtomicBool::new(false),
        )
        .unwrap();
        let repeated = bake_material_id(
            &mesh,
            64,
            &[None, None],
            1,
            64 * 64 * 4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(all, repeated);

        let filtered = bake_material_id(
            &mesh,
            64,
            &[None, Some(2)],
            1,
            64 * 64 * 4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(filtered.get_pixel(10, 10).0[3], 255);
        assert_eq!(filtered.get_pixel(40, 40).0[3], 0);
    }

    #[test]
    fn material_id_bake_rejects_invalid_bounds_and_cancellation() {
        let mesh = two_slot_id_mesh();
        assert!(bake_material_id(
            &mesh,
            32,
            &[None, None],
            1,
            u64::MAX,
            &AtomicBool::new(false),
        )
        .is_err());
        assert!(bake_material_id(
            &mesh,
            64,
            &[None, None],
            1,
            64 * 64 * 4 - 1,
            &AtomicBool::new(false),
        )
        .is_err());
        let canceled = AtomicBool::new(true);
        let error = bake_material_id(&mesh, 64, &[None, None], 1, u64::MAX, &canceled).unwrap_err();
        assert!(error.to_string().contains("canceled"));
    }

    #[test]
    fn material_id_bake_overlap_is_owned_by_first_triangle() {
        let mut mesh = two_slot_id_mesh();
        mesh.vertices.extend([
            vertex([0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.1, 0.1]),
            vertex([1.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.45, 0.1]),
            vertex([0.0, 1.0, 1.0], [0.0, 0.0, 1.0], [0.1, 0.45]),
        ]);
        mesh.indices.extend([6, 7, 8]);
        mesh.triangle_materials.push(1);
        let image = bake_material_id(
            &mesh,
            64,
            &[None, None],
            1,
            64 * 64 * 4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(image.get_pixel(10, 10).0, [0, 0, 0, 255]);
    }

    #[test]
    fn material_id_worker_delivers_result_once() {
        let mut worker = MaterialIdBakeWorker::spawn(
            two_slot_id_mesh(),
            MaterialIdBakeJobOptions {
                size: 64,
                bindings: vec![None, None],
                active_set_id: 1,
                working_budget_bytes: 64 * 64 * 4,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            if let Some(result) = worker.try_take_result() {
                break result.unwrap();
            }
            assert!(Instant::now() < deadline, "ID bake worker did not finish");
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(result.get_pixel(10, 10).0, [0, 0, 0, 255]);
        assert_eq!(result.get_pixel(40, 40).0, [1, 0, 0, 255]);
        assert!(worker.try_take_result().is_none());
    }

    #[test]
    fn bakes_analytic_position_and_world_normal_at_uv_texel() {
        let mesh = triangle_mesh();
        let maps = bake(&mesh, &[None], 1).unwrap();
        let position = maps.position(24, 24).unwrap();
        let expected = ((24.0 / 63.0) - 0.1) / 0.8;
        assert!((position[0] - expected).abs() < 1.0e-5);
        assert!((position[1] - expected).abs() < 1.0e-5);
        assert!(position[2].abs() < 1.0e-6);
        let normal = maps.world_normal(24, 24).unwrap();
        assert_eq!(normal, [0.0, 0.0, 1.0]);
        assert_eq!(
            maps.world_normal_image().get_pixel(24, 24).0,
            [128, 128, 255, 255]
        );
        let encoded_position = maps.normalized_position_image().get_pixel(24, 24).0;
        assert!((encoded_position[0] as i16 - 90).abs() <= 1);
        assert!((encoded_position[1] as i16 - 90).abs() <= 1);
        assert_eq!(encoded_position[2], 128);
        assert_eq!(encoded_position[3], 255);
        assert!(maps.covered_texels() > 0);
        assert_eq!(maps.position(63, 63), None);
    }

    #[test]
    fn texture_set_binding_filters_bake_coverage() {
        let mut mesh = triangle_mesh();
        mesh.vertices.extend([
            vertex([2.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.1, 0.1]),
            vertex([3.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.9, 0.1]),
            vertex([2.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.1, 0.9]),
        ]);
        mesh.indices.extend([3, 4, 5]);
        mesh.materials.push("Second".into());
        mesh.triangle_materials.push(1);
        let maps = bake(&mesh, &[None, Some(2)], 1).unwrap();
        assert_eq!(maps.world_normal(24, 24), Some([0.0, 0.0, 1.0]));
        assert!(maps.position(24, 24).unwrap()[0] < 1.0);
    }

    #[test]
    fn overlap_ownership_is_first_triangle_in_mesh_order() {
        let mut mesh = triangle_mesh();
        mesh.vertices.extend([
            vertex([4.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.1, 0.1]),
            vertex([5.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.9, 0.1]),
            vertex([4.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.1, 0.9]),
        ]);
        mesh.indices.extend([3, 4, 5]);
        mesh.triangle_materials.push(0);
        let maps = bake(&mesh, &[None], 1).unwrap();
        assert!(maps.position(24, 24).unwrap()[0] < 1.0);
        assert_eq!(maps.world_normal(24, 24), Some([0.0, 0.0, 1.0]));
    }

    #[test]
    fn canceled_bake_returns_no_partial_map() {
        let mesh = triangle_mesh();
        let mut checks = 0;
        let result = bake_mesh_maps_with_cancel(&mesh, 64, &[None], 1, u64::MAX, || {
            checks += 1;
            checks >= 4
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("canceled"));
    }

    #[test]
    fn invalid_size_and_binding_count_are_rejected() {
        let mesh = triangle_mesh();
        assert!(bake_mesh_maps_budgeted(
            &mesh,
            32,
            &[None],
            1,
            128 * 1024 * 1024,
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(bake_mesh_maps_budgeted(
            &mesh,
            64,
            &[],
            1,
            128 * 1024 * 1024,
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(bake_mesh_maps_budgeted(
            &mesh,
            64,
            &[None],
            1,
            64 * 64 * 29 - 1,
            &AtomicBool::new(false)
        )
        .is_err());
    }

    #[test]
    fn degenerate_uv_and_geometry_triangles_produce_no_valid_samples() {
        let mut mesh = triangle_mesh();
        mesh.vertices[1].uv = mesh.vertices[0].uv;
        mesh.vertices[2].uv = mesh.vertices[0].uv;
        let maps = bake(&mesh, &[None], 1).unwrap();
        assert_eq!(maps.covered_texels(), 0);

        let mut mesh = triangle_mesh();
        mesh.vertices[1].position = mesh.vertices[0].position;
        mesh.vertices[2].position = mesh.vertices[0].position;
        let maps = bake(&mesh, &[None], 1).unwrap();
        assert_eq!(maps.covered_texels(), 0);
    }

    #[test]
    fn extreme_finite_uvs_never_create_nonfinite_bake_samples() {
        let mut mesh = triangle_mesh();
        mesh.vertices[0].uv = [1.0e20, 1.0e20];
        mesh.vertices[1].uv = [-1.0e20, -1.0e20];
        mesh.vertices[2].uv = [1.0e20, 1.0e20 + 1.0e14];
        let maps = bake(&mesh, &[None], 1).unwrap();
        for index in 0..maps.valid.len() {
            if maps.valid[index] != 0 {
                assert!(maps.positions[index].iter().all(|value| value.is_finite()));
                assert!(maps.world_normals[index]
                    .iter()
                    .all(|value| value.is_finite()));
            }
        }
    }

    #[test]
    fn high_to_low_bake_projects_nearest_hit_along_both_low_normal_directions() {
        let low = triangle_mesh();
        let tilted = [1.0, 0.0, 1.0];
        let high_above = offset_triangle(0.25, tilted);
        let projected = bake_high_to_low_world_normals(
            &low,
            &high_above,
            HighToLowBakeOptions {
                size: 64,
                bindings: &[None],
                active_set_id: 1,
                max_ray_distance: 1.0,
                working_budget_bytes: 128 * 1024 * 1024,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let normal = projected.world_normal(24, 24).unwrap();
        let half = std::f32::consts::FRAC_1_SQRT_2;
        assert!((normal[0] - half).abs() < 1.0e-6);
        assert!(normal[1].abs() < 1.0e-6);
        assert!((normal[2] - half).abs() < 1.0e-6);

        let high_below = offset_triangle(-0.25, [0.0, 1.0, 0.0]);
        let projected = bake_high_to_low_world_normals(
            &low,
            &high_below,
            HighToLowBakeOptions {
                size: 64,
                bindings: &[None],
                active_set_id: 1,
                max_ray_distance: 1.0,
                working_budget_bytes: 128 * 1024 * 1024,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(projected.world_normal(24, 24), Some([0.0, 1.0, 0.0]));
    }

    #[test]
    fn high_to_low_bake_rejects_hits_beyond_cage_distance() {
        let low = triangle_mesh();
        let high = offset_triangle(0.25, [0.0, 0.0, 1.0]);
        let projected = bake_high_to_low_world_normals(
            &low,
            &high,
            HighToLowBakeOptions {
                size: 64,
                bindings: &[None],
                active_set_id: 1,
                max_ray_distance: 0.1,
                working_budget_bytes: 128 * 1024 * 1024,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(projected.world_normal(24, 24), None);
        assert_eq!(projected.covered_texels(), 0);
    }

    fn ao(
        mesh: &Mesh,
        bindings: &[Option<u32>],
        sample_count: u32,
        seed: u64,
    ) -> Result<BakedAmbientOcclusion> {
        bake_ambient_occlusion(
            mesh,
            AmbientOcclusionBakeOptions {
                size: 64,
                bindings,
                active_set_id: 1,
                sample_count,
                max_ray_distance: 0.5,
                seed,
                working_budget_bytes: 128 * 1024 * 1024,
            },
            &AtomicBool::new(false),
        )
    }

    fn ao_job_options(size: u32, sample_count: u32) -> AmbientOcclusionBakeJobOptions {
        AmbientOcclusionBakeJobOptions {
            size,
            bindings: vec![None],
            active_set_id: 1,
            sample_count,
            max_ray_distance: 0.5,
            seed: 17,
            working_budget_bytes: 128 * 1024 * 1024,
        }
    }

    fn wait_for_worker_result(
        worker: &mut AmbientOcclusionBakeWorker,
    ) -> Result<BakedAmbientOcclusion> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = worker.try_take_result() {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "AO worker did not finish in time"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn ambient_occlusion_worker_delivers_deterministic_result_and_row_progress() {
        let mesh = triangle_mesh();
        let expected = ao(&mesh, &[None], 4, 17).unwrap();
        let mut worker = AmbientOcclusionBakeWorker::spawn(mesh, ao_job_options(64, 4)).unwrap();
        let result = wait_for_worker_result(&mut worker).unwrap();

        assert_eq!(worker.progress().total_rows, 64);
        assert_eq!(worker.progress().completed_rows, 64);
        assert_eq!(result.occlusion, expected.occlusion);
        assert_eq!(result.valid, expected.valid);
        assert!(
            worker.try_take_result().is_none(),
            "result is delivered once"
        );
    }

    #[test]
    fn ambient_occlusion_worker_cancellation_is_nonblocking_and_returns_error() {
        let mesh = triangle_mesh();
        let mut options = ao_job_options(1024, 64);
        options.working_budget_bytes = 256 * 1024 * 1024;
        let mut worker = AmbientOcclusionBakeWorker::spawn(mesh, options).unwrap();
        worker.cancel();

        let error = wait_for_worker_result(&mut worker).unwrap_err();
        assert!(error.to_string().contains("canceled"));
        assert!(worker.progress().completed_rows <= worker.progress().total_rows);
    }

    #[test]
    fn ambient_occlusion_plane_without_blocker_is_open_and_seed_deterministic() {
        let mesh = triangle_mesh();
        let first = ao(&mesh, &[None], 64, 17).unwrap();
        let repeated = ao(&mesh, &[None], 64, 17).unwrap();
        assert!(first.is_valid(24, 24));
        assert_eq!(first.occlusion[24 * 64 + 24], 0.0);
        assert_eq!(first.occlusion, repeated.occlusion);
        assert_eq!(first.valid, repeated.valid);
        assert!(!first.is_valid(63, 63));

        let image = first.occlusion_image().unwrap();
        assert_eq!(image.get_pixel(24, 24).0, [255, 255, 255, 255]);
        assert_eq!(image.get_pixel(63, 63).0, [255, 255, 255, 0]);
    }

    #[test]
    fn ambient_occlusion_counts_hits_on_blocker_in_another_texture_set() {
        let mut mesh = triangle_mesh();
        mesh.vertices.extend([
            vertex([-2.0, -2.0, 0.25], [0.0, 0.0, 1.0], [2.0, 2.0]),
            vertex([2.0, -2.0, 0.25], [0.0, 0.0, 1.0], [3.0, 2.0]),
            vertex([2.0, 2.0, 0.25], [0.0, 0.0, 1.0], [3.0, 3.0]),
            vertex([-2.0, 2.0, 0.25], [0.0, 0.0, 1.0], [2.0, 3.0]),
        ]);
        mesh.indices.extend([3, 4, 5, 3, 5, 6]);
        mesh.materials.push("Occluder".into());
        mesh.triangle_materials.extend([1, 1]);

        let maps = ao(&mesh, &[None, Some(2)], 512, 99).unwrap();
        let value = maps.occlusion[24 * 64 + 24];
        assert!(maps.is_valid(24, 24));
        // For a large parallel blocker at one quarter of the ray range, a
        // cosine-weighted hemisphere predicts about 75% of rays reach it.
        assert!((0.65..=0.85).contains(&value), "occlusion={value}");
        assert!(!maps.is_valid(63, 63));

        let image = maps.occlusion_image().unwrap();
        let encoded = image.get_pixel(24, 24).0;
        let expected = ((1.0 - value) * 255.0).round() as u8;
        assert_eq!(encoded, [expected, expected, expected, 255]);
        assert!((38..=90).contains(&encoded[0]), "encoded AO={encoded:?}");
        assert_eq!(image.get_pixel(63, 63).0, [255, 255, 255, 0]);
    }

    #[test]
    fn ambient_occlusion_image_clamps_finite_and_safely_handles_nonfinite_values() {
        let mut values = vec![0.0; 64 * 64];
        let mut valid = vec![0; 64 * 64];
        for (index, value) in [0.0, 0.5, 1.0, -0.25, 1.25, f32::NAN]
            .into_iter()
            .enumerate()
        {
            values[index] = value;
            valid[index] = 1;
        }
        let image = BakedAmbientOcclusion {
            size: 64,
            occlusion: values,
            valid,
        }
        .occlusion_image()
        .unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [128, 128, 128, 255]);
        assert_eq!(image.get_pixel(2, 0).0, [0, 0, 0, 255]);
        assert_eq!(image.get_pixel(3, 0).0, [255, 255, 255, 255]);
        assert_eq!(image.get_pixel(4, 0).0, [0, 0, 0, 255]);
        assert_eq!(image.get_pixel(5, 0).0, [255, 255, 255, 255]);
        assert_eq!(image.get_pixel(6, 0).0, [255, 255, 255, 0]);

        let malformed = BakedAmbientOcclusion {
            size: 64,
            occlusion: vec![0.0],
            valid: vec![0],
        };
        assert!(malformed.occlusion_image().is_err());
    }

    #[test]
    fn ambient_occlusion_enforces_sample_memory_and_cancel_bounds() {
        let mesh = triangle_mesh();
        let mut options = AmbientOcclusionBakeOptions {
            size: 64,
            bindings: &[None],
            active_set_id: 1,
            sample_count: 0,
            max_ray_distance: 1.0,
            seed: 0,
            working_budget_bytes: 128 * 1024 * 1024,
        };
        assert!(bake_ambient_occlusion(&mesh, options, &AtomicBool::new(false)).is_err());

        options.sample_count = MAX_AO_SAMPLES_PER_TEXEL + 1;
        assert!(bake_ambient_occlusion(&mesh, options, &AtomicBool::new(false)).is_err());

        options.sample_count = 1;
        options.working_budget_bytes = 64 * 64 * 34 - 1;
        assert!(bake_ambient_occlusion(&mesh, options, &AtomicBool::new(false)).is_err());

        options.working_budget_bytes = 128 * 1024 * 1024;
        let canceled = AtomicBool::new(true);
        let error = bake_ambient_occlusion(&mesh, options, &canceled).unwrap_err();
        assert!(error.to_string().contains("canceled"));
    }

    #[test]
    fn material_id_outputs_write_matching_png_and_metadata_and_replace_as_a_pair() {
        let output = std::env::temp_dir().join(format!(
            "kalium-material-id-output-{}.png",
            uuid::Uuid::new_v4()
        ));
        let mesh = triangle_mesh();
        let image =
            bake_material_id(&mesh, 64, &[None], 7, 64 * 64 * 4, &AtomicBool::new(false)).unwrap();
        let metadata = material_id_metadata(&mesh, 64, 7, "Leather", true).unwrap();
        let sidecar = write_material_id_outputs(&image, &metadata, &output).unwrap();
        let decoded = image::open(&output).unwrap().to_rgba8();
        assert_eq!(decoded, image);
        assert_eq!(sidecar, PathBuf::from(format!("{}.json", output.display())));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&sidecar).unwrap()).unwrap(),
            metadata
        );

        let replacement = RgbaImage::from_pixel(64, 64, Rgba([9, 8, 7, 6]));
        let replacement_metadata = material_id_metadata(&mesh, 64, 7, "Leather", true).unwrap();
        write_material_id_outputs(&replacement, &replacement_metadata, &output).unwrap();
        assert_eq!(image::open(&output).unwrap().to_rgba8(), replacement);
        let _ = fs::remove_file(&output);
        let _ = fs::remove_file(sidecar);
    }

    #[test]
    fn material_id_output_failure_restores_existing_png_and_refuses_sidecar_directory() {
        let output = std::env::temp_dir().join(format!(
            "kalium-material-id-output-failure-{}.png",
            uuid::Uuid::new_v4()
        ));
        let sidecar = PathBuf::from(format!("{}.json", output.display()));
        let old_png = b"previous png bytes";
        fs::write(&output, old_png).unwrap();
        fs::create_dir(&sidecar).unwrap();
        let image = RgbaImage::from_pixel(64, 64, Rgba([1, 2, 3, 4]));

        assert!(write_material_id_outputs(&image, &serde_json::json!({}), &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), old_png);
        assert!(sidecar.is_dir());

        let _ = fs::remove_file(output);
        let _ = fs::remove_dir_all(sidecar);
    }

    #[test]
    fn high_to_low_output_failure_restores_existing_bundle_members() {
        let output_dir = std::env::temp_dir().join(format!(
            "kalium-high-to-low-output-failure-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&output_dir).unwrap();
        let normal_path = output_dir.join("Kalium_HighToLow_WorldNormal.png");
        let coverage_path = output_dir.join("Kalium_HighToLow_Coverage.png");
        let old_normal = b"previous normal bytes";
        fs::write(&normal_path, old_normal).unwrap();
        fs::create_dir(&coverage_path).unwrap();
        let maps = BakedMeshMaps {
            size: 1,
            positions: vec![[0.0, 0.0, 0.0]],
            world_normals: vec![[0.0, 0.0, 1.0]],
            valid: vec![1],
            position_min: [0.0; 3],
            position_max: [1.0; 3],
        };

        assert!(write_high_to_low_outputs(&maps, &serde_json::json!({}), &output_dir).is_err());
        assert_eq!(fs::read(&normal_path).unwrap(), old_normal);
        assert!(coverage_path.is_dir());
        assert!(!output_dir.join("Kalium_HighToLow_Metadata.json").exists());
        assert_eq!(
            fs::read_dir(&output_dir).unwrap().count(),
            2,
            "staged and backup files should be cleaned after a publication failure"
        );
        let _ = fs::remove_dir_all(output_dir);
    }
}

use crate::document::{Channel, Document};
use crate::uv::UvCoverage;
use anyhow::{anyhow, Context, Result};
use image::{Rgba, RgbaImage};
use std::path::{Path, PathBuf};

/// Export settings for per-texture-set maps.
///
/// `padding_radius` is measured in texels. Padding copies the nearest texel
/// covered by that texture set's UVs into uncovered texels, up to this radius.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Optional square output resolution. `None` preserves each set's working size.
    pub output_size: Option<u32>,
    pub padding_radius: u32,
    pub normal_y: NormalYConvention,
    pub profile: EngineExportProfile,
}

/// Named engine-facing export layout. `Custom` preserves the regular map set
/// and uses [`ExportOptions::normal_y`]; engine profiles pin conventions and
/// write a machine-readable sidecar alongside their maps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EngineExportProfile {
    #[default]
    Custom,
    Godot4Orm,
    UnityMetallic,
    GltfMetallicRoughness,
    SubstancePainterPython,
}

impl EngineExportProfile {
    pub const ALL: [Self; 5] = [
        Self::Custom,
        Self::Godot4Orm,
        Self::UnityMetallic,
        Self::GltfMetallicRoughness,
        Self::SubstancePainterPython,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Custom => "Custom / generic PBR maps",
            Self::Godot4Orm => "Godot 4 / ORM (+Y normal)",
            Self::UnityMetallic => "Unity / metallic-smoothness (+Y normal)",
            Self::GltfMetallicRoughness => "glTF 2.0 / metallic-roughness (+Y normal)",
            Self::SubstancePainterPython => "Substance Painter / export preset JSON",
        }
    }

    pub const fn normal_y(self, custom: NormalYConvention) -> NormalYConvention {
        match self {
            Self::Custom => custom,
            Self::Godot4Orm | Self::UnityMetallic | Self::GltfMetallicRoughness => {
                NormalYConvention::OpenGl
            }
            Self::SubstancePainterPython => NormalYConvention::OpenGl,
        }
    }
}

/// Convention used for the green (tangent-space Y) channel in exported
/// Normal maps. OpenGL is the internal/default convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NormalYConvention {
    #[default]
    OpenGl,
    DirectX,
}

impl NormalYConvention {
    pub const ALL: [Self; 2] = [Self::OpenGl, Self::DirectX];

    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenGl => "OpenGL (+Y)",
            Self::DirectX => "DirectX (−Y)",
        }
    }
}

/// Bound the optional padding radius even though the distance transform runs
/// in linear time. This keeps exports within a practical UV gutter size.
pub const MAX_PADDING_RADIUS: u32 = 256;

/// Supported export resolutions. This cap bounds the largest individual
/// output image to 64 MiB of RGBA8 pixels.
pub const EXPORT_SIZES: [u32; 7] = [64, 128, 256, 512, 1024, 2048, 4096];

const MAP_FILENAMES: [&str; 11] = [
    "Kalium_BaseColor.png",
    "Kalium_Roughness.png",
    "Kalium_Metallic.png",
    "Kalium_Normal.png",
    "Kalium_Height.png",
    "Kalium_Emissive.png",
    "Kalium_Opacity.png",
    "Kalium_AmbientOcclusion.png",
    "Kalium_BaseColor_Alpha.png",
    "Kalium_ORM_Unreal_Godot.png",
    "Kalium_MetallicSmoothness_Unity.png",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExportSetSnapshot {
    id: u32,
    name: String,
    source_size: u32,
    output_size: u32,
}

fn write_profile_manifest(
    profile: EngineExportProfile,
    dir: &Path,
    set: &ExportSetSnapshot,
) -> Result<()> {
    if profile == EngineExportProfile::SubstancePainterPython {
        let path = dir.join("Kalium_SubstancePainter_ExportPreset.json");
        let preset = substance_painter_export_preset();
        std::fs::write(&path, serde_json::to_vec_pretty(&preset)?)
            .with_context(|| format!("Writing {}", path.display()))?;
        return Ok(());
    }
    let (filename, manifest) = match profile {
        EngineExportProfile::Custom => return Ok(()),
        EngineExportProfile::Godot4Orm => (
            "Kalium_Godot4_Profile.json",
            serde_json::json!({
                "format_version": 1,
                "target": "Godot 4",
                "material_type": "ORMMaterial3D",
                "texture_set_id": set.id,
                "texture_set_name": set.name,
                "resolution": [set.output_size, set.output_size],
                "textures": {
                    "albedo": "Kalium_BaseColor.png",
                    "normal": "Kalium_Normal.png",
                    "orm": "Kalium_ORM_Unreal_Godot.png",
                    "emission": "Kalium_Emissive.png",
                    "opacity": "Kalium_Opacity.png"
                },
                "orm_channels": {
                    "red": "occlusion",
                    "green": "roughness",
                    "blue": "metallic"
                },
                "normal_map_convention": "OpenGL (+Y)",
                "notes": "Import Normal as a normal map; set normal_enabled when using StandardMaterial3D. The ORM texture uses linear data channels."
            }),
        ),
        EngineExportProfile::UnityMetallic => (
            "Kalium_Unity_Profile.json",
            serde_json::json!({
                "format_version": 1,
                "target": "Unity",
                "material_workflow": "metallic",
                "texture_set_id": set.id,
                "texture_set_name": set.name,
                "resolution": [set.output_size, set.output_size],
                "textures": {
                    "albedo": "Kalium_BaseColor.png",
                    "normal": "Kalium_Normal.png",
                    "metallic_smoothness": "Kalium_MetallicSmoothness_Unity.png",
                    "emission": "Kalium_Emissive.png",
                    "opacity": "Kalium_Opacity.png"
                },
                "metallic_smoothness_channels": {
                    "red": "metallic",
                    "alpha": "smoothness (1 - roughness)"
                },
                "normal_map_convention": "OpenGL (+Y)",
                "normal_import_type": "Normal map; Flip Green Channel = false",
                "notes": "Assign the metallic-smoothness texture to the metallic map slot; enable alpha as the smoothness source. Select transparency separately if the material uses opacity."
            }),
        ),
        EngineExportProfile::GltfMetallicRoughness => (
            "Kalium_glTF_Profile.json",
            serde_json::json!({
                "format_version": 1,
                "target": "glTF 2.0",
                "material_workflow": "metallic-roughness",
                "texture_set_id": set.id,
                "texture_set_name": set.name,
                "resolution": [set.output_size, set.output_size],
                "textures": {
                    "base_color": "Kalium_BaseColor_Alpha.png",
                    "normal": "Kalium_Normal.png",
                    "metallic_roughness": "Kalium_ORM_Unreal_Godot.png",
                    "occlusion": "Kalium_ORM_Unreal_Godot.png",
                    "emissive": "Kalium_Emissive.png",
                    "opacity": "Kalium_Opacity.png"
                },
                "metallic_roughness_channels": {
                    "green": "roughness",
                    "blue": "metallic",
                    "red": "ignored by metallic-roughness; used for occlusion"
                },
                "occlusion_channel": "red",
                "normal_map_convention": "OpenGL (+Y)",
                "color_spaces": {
                    "base_color": "sRGB",
                    "base_color_alpha": "linear opacity coverage",
                    "emissive": "sRGB",
                    "metallic_roughness": "linear",
                    "occlusion": "linear",
                    "normal": "linear"
                },
                "notes": "This sidecar describes texture assignments; it is not a glTF material or model file. The packed ORM image may be referenced by both metallicRoughnessTexture and occlusionTexture. Configure alphaMode and alphaCutoff separately when opacity is used."
            }),
        ),
        EngineExportProfile::SubstancePainterPython => unreachable!("handled above"),
    };
    let path = dir.join(filename);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("Writing {}", path.display()))?;
    Ok(())
}

/// Public Substance Painter Python API preset shape (`name` + `maps`). This
/// is an API payload, not an undocumented `.spexp` shelf resource or `.spp`.
fn substance_painter_export_preset() -> serde_json::Value {
    fn map(file_name: &str, source: &str, color: bool) -> serde_json::Value {
        let channels = if color {
            ["R", "G", "B"]
                .into_iter()
                .map(|channel| {
                    serde_json::json!({
                        "destChannel": channel,
                        "srcChannel": channel,
                        "srcMapType": "documentMap",
                        "srcMapName": source
                    })
                })
                .collect::<Vec<_>>()
        } else {
            vec![serde_json::json!({
                "destChannel": "L",
                "srcChannel": "L",
                "srcMapType": "documentMap",
                "srcMapName": source
            })]
        };
        serde_json::json!({
            "fileName": file_name,
            "channels": channels,
            "parameters": {
                "fileFormat": "png",
                "bitDepth": "8",
                "dithering": false
            }
        })
    }
    serde_json::json!({
        "name": "Kalium PBR texture maps",
        "maps": [
            map("$mesh_$textureSet_BaseColor", "basecolor", true),
            map("$mesh_$textureSet_Roughness", "roughness", false),
            map("$mesh_$textureSet_Metallic", "metallic", false),
            map("$mesh_$textureSet_Normal", "normal", true),
            map("$mesh_$textureSet_Height", "height", false),
            map("$mesh_$textureSet_Emissive", "emissive", true),
            map("$mesh_$textureSet_Opacity", "opacity", false),
            map("$mesh_$textureSet_AmbientOcclusion", "ambientOcclusion", false)
        ]
    })
}

struct PaddingContext {
    coverage: UvCoverage,
    nearest: Vec<u32>,
}

/// Status returned after one frame-stepped export action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportJobState {
    Running,
    Complete,
    Cancelled,
}

/// Progress from one call to [`ExportJob::step`]. Indices are one-based when
/// `current` is present. A cancelled job can leave already-written files in
/// the output tree; callers should present that directory as partial output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportJobProgress {
    pub state: ExportJobState,
    pub completed_maps: usize,
    pub total_maps: usize,
    pub current: Option<ExportMapProgress>,
}

/// The set and logical PNG map written by the most recent step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportMapProgress {
    pub set_index: usize,
    pub set_count: usize,
    pub set_id: u32,
    pub set_name: String,
    pub map_index: usize,
    pub map_count: usize,
    pub map_filename: &'static str,
}

/// A frame-stepped export. `new` exports every set into a numbered folder;
/// `new_active` exports only the active set with the legacy flat filenames. It
/// snapshots only stable set identity, name, and size; composites are read
/// from the document on each step. Each call to `step` writes no more than one
/// PNG. Cancelling leaves files written by prior steps in place as partial
/// output.
pub struct ExportJob {
    output_root: PathBuf,
    flat_output: bool,
    options: ExportOptions,
    sets: Vec<ExportSetSnapshot>,
    completed_maps: usize,
    cancelled: bool,
    padding: Option<(usize, PaddingContext)>,
}

impl ExportJob {
    pub fn new(
        doc: &Document,
        output_root: impl AsRef<Path>,
        options: ExportOptions,
    ) -> Result<Self> {
        Self::new_with_layout(doc, output_root, options, false)
    }

    /// Create a stepped export for only the active Texture Set, using the
    /// legacy flat `Kalium_*.png` filenames used by [`export_textures_with_options`].
    pub fn new_active(
        doc: &Document,
        output_root: impl AsRef<Path>,
        options: ExportOptions,
    ) -> Result<Self> {
        Self::new_with_layout(doc, output_root, options, true)
    }

    fn new_with_layout(
        doc: &Document,
        output_root: impl AsRef<Path>,
        options: ExportOptions,
        active_only: bool,
    ) -> Result<Self> {
        validate_output_size(options.output_size)?;
        if options.padding_radius > MAX_PADDING_RADIUS {
            return Err(anyhow!(
                "UV padding radius must be at most {MAX_PADDING_RADIUS} texels"
            ));
        }
        doc.validate()?;
        let listed_sets = doc.texture_set_list();
        let selected_sets = if active_only {
            vec![listed_sets
                .into_iter()
                .find(|(_, _, _, active)| *active)
                .ok_or_else(|| anyhow!("Document has no active Texture Set"))?]
        } else {
            listed_sets
        };
        let sets = selected_sets
            .into_iter()
            .map(|(id, name, source_size, _)| ExportSetSnapshot {
                id,
                name,
                source_size,
                output_size: options.output_size.unwrap_or(source_size),
            })
            .collect();
        Ok(Self {
            output_root: output_root.as_ref().to_path_buf(),
            flat_output: active_only,
            options,
            sets,
            completed_maps: 0,
            cancelled: false,
            padding: None,
        })
    }

    /// Request cancellation. The next and all later calls to `step` report
    /// `Cancelled`, even if the job had already completed.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.padding = None;
    }

    /// Write at most one PNG map for one Texture Set and report progress.
    pub fn step(&mut self, doc: &Document) -> Result<ExportJobProgress> {
        if self.cancelled {
            return Ok(self.progress(ExportJobState::Cancelled, None));
        }
        let total_maps = self.sets.len() * MAP_FILENAMES.len();
        if self.completed_maps >= total_maps {
            return Ok(self.progress(ExportJobState::Complete, None));
        }

        let set_index = self.completed_maps / MAP_FILENAMES.len();
        let map_index = self.completed_maps % MAP_FILENAMES.len();
        let set = &self.sets[set_index];
        let exists_at_size = doc
            .texture_set_list()
            .into_iter()
            .any(|(id, _, size, _)| id == set.id && size == set.source_size);
        if !exists_at_size {
            return Err(anyhow!(
                "Texture Set {} is missing or changed size during export",
                set.id
            ));
        }

        let directory = if self.flat_output {
            self.output_root.clone()
        } else {
            self.output_root.join(set_folder_name(set.id, &set.name))
        };
        let composite = |channel| {
            let image = doc
                .composite_for_set(set.id, channel)
                .ok_or_else(|| anyhow!("Unknown texture set {}", set.id))?;
            resample_export_image(channel, image, set.output_size)
        };
        if self.options.padding_radius == 0 {
            write_one_map(
                set.output_size,
                &directory,
                map_index,
                self.options.profile.normal_y(self.options.normal_y),
                composite,
            )?;
        } else {
            if self
                .padding
                .as_ref()
                .is_none_or(|(cached_set, _)| *cached_set != set_index)
            {
                let coverage = UvCoverage::generate_for_set(
                    &doc.mesh,
                    set.output_size,
                    &doc.material_bindings,
                    set.id,
                )?;
                let nearest = nearest_covered_texels(&coverage);
                self.padding = Some((set_index, PaddingContext { coverage, nearest }));
            }
            let (_, padding) = self.padding.as_ref().expect("padding initialized");
            write_one_map(
                set.output_size,
                &directory,
                map_index,
                self.options.profile.normal_y(self.options.normal_y),
                |channel| {
                    let mut image = composite(channel)?;
                    pad_image(
                        &mut image,
                        &padding.coverage,
                        &padding.nearest,
                        self.options.padding_radius,
                    );
                    Ok(image)
                },
            )?;
        }

        self.completed_maps += 1;
        if map_index + 1 == MAP_FILENAMES.len() {
            write_profile_manifest(self.options.profile, &directory, set)?;
        }
        let current = Some(ExportMapProgress {
            set_index: set_index + 1,
            set_count: self.sets.len(),
            set_id: set.id,
            set_name: set.name.clone(),
            map_index: map_index + 1,
            map_count: MAP_FILENAMES.len(),
            map_filename: MAP_FILENAMES[map_index],
        });
        let state = if self.completed_maps == total_maps {
            ExportJobState::Complete
        } else {
            ExportJobState::Running
        };
        if map_index + 1 == MAP_FILENAMES.len() {
            self.padding = None;
        }
        Ok(self.progress(state, current))
    }

    fn progress(
        &self,
        state: ExportJobState,
        current: Option<ExportMapProgress>,
    ) -> ExportJobProgress {
        ExportJobProgress {
            state,
            completed_maps: self.completed_maps,
            total_maps: self.sets.len() * MAP_FILENAMES.len(),
            current,
        }
    }
}

fn write_maps<F>(
    size: u32,
    dir: &Path,
    normal_y: NormalYConvention,
    mut composite: F,
) -> Result<usize>
where
    F: FnMut(Channel) -> Result<RgbaImage>,
{
    std::fs::create_dir_all(dir)?;
    for map_index in 0..MAP_FILENAMES.len() {
        write_one_map(size, dir, map_index, normal_y, &mut composite)?;
    }
    Ok(11)
}

fn write_one_map<F>(
    size: u32,
    dir: &Path,
    map_index: usize,
    normal_y: NormalYConvention,
    mut composite: F,
) -> Result<()>
where
    F: FnMut(Channel) -> Result<RgbaImage>,
{
    if map_index >= MAP_FILENAMES.len() {
        return Err(anyhow!("Unknown export map index {map_index}"));
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(MAP_FILENAMES[map_index]);
    match map_index {
        0..=7 => {
            let channel = Channel::ALL[map_index];
            let mut image = composite(channel)?;
            if channel == Channel::Normal && normal_y == NormalYConvention::DirectX {
                invert_normal_green(&mut image);
            }
            image
                .save(&path)
                .with_context(|| format!("Writing {}", path.display()))?;
        }
        8 => {
            let mut base = composite(Channel::BaseColor)?;
            let opacity = composite(Channel::Opacity)?;
            for (base, opa) in base.pixels_mut().zip(opacity.pixels()) {
                base.0[3] = opa.0[0];
            }
            base.save(&path)
                .with_context(|| format!("Writing {}", path.display()))?;
        }
        9 | 10 => {
            let metal = composite(Channel::Metallic)?;
            let rough = composite(Channel::Roughness)?;
            let ao = (map_index == 9)
                .then(|| composite(Channel::Occlusion))
                .transpose()?;
            let mut image = RgbaImage::new(size, size);
            for y in 0..size {
                for x in 0..size {
                    let r = rough.get_pixel(x, y).0[0];
                    let m = metal.get_pixel(x, y).0[0];
                    let pixel = if let Some(ao) = ao.as_ref() {
                        Rgba([ao.get_pixel(x, y).0[0], r, m, 255])
                    } else {
                        Rgba([m, 0, 0, 255 - r])
                    };
                    image.put_pixel(x, y, pixel);
                }
            }
            image
                .save(&path)
                .with_context(|| format!("Writing {}", path.display()))?;
        }
        _ => unreachable!("map index checked above"),
    }
    Ok(())
}

fn validate_output_size(size: Option<u32>) -> Result<()> {
    if let Some(size) = size {
        if !EXPORT_SIZES.contains(&size) {
            return Err(anyhow!(
                "Export resolution must be one of: {}",
                EXPORT_SIZES
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(())
}

/// Resize one already-composited map. Downscales use an area filter and
/// upscales use bilinear filtering. Color maps are filtered in linear light;
/// normals are averaged as vectors then normalized; data maps use byte-space
/// interpolation. A same-size request returns the original bytes unchanged.
fn resample_export_image(
    channel: Channel,
    image: RgbaImage,
    output_size: u32,
) -> Result<RgbaImage> {
    if image.width() == output_size && image.height() == output_size {
        return Ok(image);
    }
    let source_width = image.width();
    let source_height = image.height();
    let pixel_count = (output_size as usize)
        .checked_mul(output_size as usize)
        .ok_or_else(|| anyhow!("Export image dimensions overflow"))?;
    let byte_count = pixel_count
        .checked_mul(4)
        .ok_or_else(|| anyhow!("Export image allocation overflow"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(byte_count)
        .map_err(|error| anyhow!("Allocating export image: {error}"))?;
    bytes.resize(byte_count, 0);

    let mode = if matches!(channel, Channel::BaseColor | Channel::Emissive) {
        ResampleMode::SrgbColor
    } else if channel == Channel::Normal {
        ResampleMode::Normal
    } else {
        ResampleMode::Data
    };

    for output_y in 0..output_size {
        for output_x in 0..output_size {
            let value = if output_size < source_width || output_size < source_height {
                area_sample(&image, output_x, output_y, output_size, mode)
            } else {
                bilinear_sample(&image, output_x, output_y, output_size, mode)
            };
            let index = ((output_y as usize * output_size as usize) + output_x as usize) * 4;
            for component in 0..4 {
                bytes[index + component] = value[component].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    RgbaImage::from_raw(output_size, output_size, bytes)
        .ok_or_else(|| anyhow!("Could not construct resized export image"))
}

#[derive(Clone, Copy)]
enum ResampleMode {
    Data,
    SrgbColor,
    Normal,
}

fn decode_component(pixel: &Rgba<u8>, component: usize, mode: ResampleMode) -> f64 {
    match mode {
        ResampleMode::SrgbColor if component < 3 => {
            srgb_to_linear(pixel.0[component] as f64 / 255.0)
        }
        ResampleMode::Normal if component < 3 => pixel.0[component] as f64 / 127.5 - 1.0,
        _ => pixel.0[component] as f64 / 255.0,
    }
}

fn encode_components(mut value: [f64; 4], mode: ResampleMode) -> [f64; 4] {
    if let ResampleMode::Normal = mode {
        let length = (value[0] * value[0] + value[1] * value[1] + value[2] * value[2]).sqrt();
        if length > 1.0e-12 {
            for component in &mut value[..3] {
                *component /= length;
            }
        } else {
            value[..3].copy_from_slice(&[0.0, 0.0, 1.0]);
        }
        for component in &mut value[..3] {
            *component = (*component + 1.0) * 127.5;
        }
        value[3] *= 255.0;
    } else {
        for (component, value) in value.iter_mut().enumerate() {
            *value = if matches!(mode, ResampleMode::SrgbColor) && component < 3 {
                linear_to_srgb(*value) * 255.0
            } else {
                *value * 255.0
            };
        }
    }
    value
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn bilinear_sample(
    image: &RgbaImage,
    x: u32,
    y: u32,
    output_size: u32,
    mode: ResampleMode,
) -> [f64; 4] {
    let sx = ((x as f64 + 0.5) * image.width() as f64 / output_size as f64 - 0.5)
        .clamp(0.0, image.width().saturating_sub(1) as f64);
    let sy = ((y as f64 + 0.5) * image.height() as f64 / output_size as f64 - 0.5)
        .clamp(0.0, image.height().saturating_sub(1) as f64);
    let x0 = sx.floor() as u32;
    let y0 = sy.floor() as u32;
    let x1 = (x0 + 1).min(image.width() - 1);
    let y1 = (y0 + 1).min(image.height() - 1);
    let tx = sx - x0 as f64;
    let ty = sy - y0 as f64;
    let mut sum = [0.0; 4];
    for (px, py, weight) in [
        (x0, y0, (1.0 - tx) * (1.0 - ty)),
        (x1, y0, tx * (1.0 - ty)),
        (x0, y1, (1.0 - tx) * ty),
        (x1, y1, tx * ty),
    ] {
        let pixel = image.get_pixel(px, py);
        for (component, value) in sum.iter_mut().enumerate() {
            *value += decode_component(pixel, component, mode) * weight;
        }
    }
    encode_components(sum, mode)
}

fn area_sample(
    image: &RgbaImage,
    x: u32,
    y: u32,
    output_size: u32,
    mode: ResampleMode,
) -> [f64; 4] {
    let x0 = x as f64 * image.width() as f64 / output_size as f64;
    let x1 = (x + 1) as f64 * image.width() as f64 / output_size as f64;
    let y0 = y as f64 * image.height() as f64 / output_size as f64;
    let y1 = (y + 1) as f64 * image.height() as f64 / output_size as f64;
    let mut sum = [0.0; 4];
    let mut total_weight = 0.0;
    for source_y in y0.floor() as u32..(y1.ceil() as u32).min(image.height()) {
        let wy = (y1.min(source_y as f64 + 1.0) - y0.max(source_y as f64)).max(0.0);
        for source_x in x0.floor() as u32..(x1.ceil() as u32).min(image.width()) {
            let wx = (x1.min(source_x as f64 + 1.0) - x0.max(source_x as f64)).max(0.0);
            let weight = wx * wy;
            let pixel = image.get_pixel(source_x, source_y);
            for (component, value) in sum.iter_mut().enumerate() {
                *value += decode_component(pixel, component, mode) * weight;
            }
            total_weight += weight;
        }
    }
    if total_weight > 0.0 {
        for component in &mut sum {
            *component /= total_weight;
        }
    }
    encode_components(sum, mode)
}

fn set_folder_name(id: u32, name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    format!("{id:03}_{safe}")
}

fn write_padded_maps<F>(
    size: u32,
    dir: &Path,
    radius: u32,
    normal_y: NormalYConvention,
    coverage: &UvCoverage,
    nearest: &[u32],
    mut composite: F,
) -> Result<usize>
where
    F: FnMut(Channel) -> Result<RgbaImage>,
{
    write_maps(size, dir, normal_y, |channel| {
        let mut image = composite(channel)?;
        pad_image(&mut image, coverage, nearest, radius);
        Ok(image)
    })
}

fn invert_normal_green(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        pixel.0[1] = 255 - pixel.0[1];
    }
}

/// Computes a deterministic nearest covered texel index for every atlas
/// texel using a separable squared Euclidean distance transform. The lower
/// linear source index wins equal-distance ties.
fn nearest_covered_texels(coverage: &UvCoverage) -> Vec<u32> {
    let size = coverage.size as usize;
    let count = size * size;
    let mut horizontal = vec![u32::MAX; count];

    for y in 0..size {
        let row = y * size;
        let mut left = None;
        for x in 0..size {
            let index = row + x;
            if coverage.contains(x as u32, y as u32) {
                left = Some(index as u32);
            }
            horizontal[index] = left.unwrap_or(u32::MAX);
        }
        let mut right = None;
        for x in (0..size).rev() {
            let index = row + x;
            if coverage.contains(x as u32, y as u32) {
                right = Some(index as u32);
            }
            if let Some(candidate) = right {
                let current = horizontal[index];
                let candidate_x = (candidate as usize) % size;
                let current_x = if current == u32::MAX {
                    usize::MAX
                } else {
                    (current as usize) % size
                };
                let candidate_distance = candidate_x.abs_diff(x);
                let current_distance = current_x.abs_diff(x);
                if candidate_distance < current_distance
                    || (candidate_distance == current_distance && candidate < current)
                {
                    horizontal[index] = candidate;
                }
            }
        }
    }

    let mut envelope = Vec::<usize>::with_capacity(size);
    let mut envelope_sources = Vec::<u32>::with_capacity(size);
    let mut boundaries = Vec::<f64>::with_capacity(size + 1);
    for x in 0..size {
        envelope.clear();
        envelope_sources.clear();
        boundaries.clear();
        boundaries.push(f64::NEG_INFINITY);
        for y in 0..size {
            let source = horizontal[y * size + x];
            if source == u32::MAX {
                continue;
            }
            let source_x = source as usize % size;
            let dx = source_x.abs_diff(x) as f64;
            let cost = dx * dx;
            while let Some(&previous_y) = envelope.last() {
                let previous_source = *envelope_sources.last().unwrap();
                let previous_x = previous_source as usize % size;
                let previous_dx = previous_x.abs_diff(x) as f64;
                let previous_cost = previous_dx * previous_dx;
                let intersection =
                    (cost + (y * y) as f64 - previous_cost - (previous_y * previous_y) as f64)
                        / (2.0 * (y - previous_y) as f64);
                if intersection <= *boundaries.last().unwrap() {
                    envelope.pop();
                    envelope_sources.pop();
                    boundaries.pop();
                } else {
                    boundaries.push(intersection);
                    break;
                }
            }
            if envelope.is_empty() {
                boundaries.clear();
                boundaries.push(f64::NEG_INFINITY);
            }
            envelope.push(y);
            envelope_sources.push(source);
        }
        if envelope.is_empty() {
            continue;
        }
        boundaries.push(f64::INFINITY);
        let mut segment = 0usize;
        for y in 0..size {
            while boundaries[segment + 1] < y as f64 {
                segment += 1;
            }
            horizontal[y * size + x] = envelope_sources[segment];
        }
    }
    horizontal
}

fn pad_image(image: &mut RgbaImage, coverage: &UvCoverage, nearest: &[u32], radius: u32) {
    if radius == 0 {
        return;
    }
    let size = coverage.size as usize;
    let radius_squared = u64::from(radius) * u64::from(radius);
    for y in 0..size {
        for x in 0..size {
            if coverage.contains(x as u32, y as u32) {
                continue;
            }
            let index = y * size + x;
            let source = nearest[index];
            if source == u32::MAX {
                continue;
            }
            let source = source as usize;
            let dx = (source % size).abs_diff(x) as u64;
            let dy = (source / size).abs_diff(y) as u64;
            if dx * dx + dy * dy <= radius_squared {
                image.put_pixel(
                    x as u32,
                    y as u32,
                    *image.get_pixel((source % size) as u32, (source / size) as u32),
                );
            }
        }
    }
}

/// Exports the active texture set (preserves the legacy output naming).
pub fn export_textures(doc: &Document, dir: &Path) -> Result<usize> {
    export_textures_with_options(doc, dir, ExportOptions::default())
}

/// Export the active texture set using the requested Normal map convention.
pub fn export_textures_with_options(
    doc: &Document,
    dir: &Path,
    options: ExportOptions,
) -> Result<usize> {
    validate_output_size(options.output_size)?;
    if options.padding_radius > MAX_PADDING_RADIUS {
        return Err(anyhow!(
            "UV padding radius must be at most {MAX_PADDING_RADIUS} texels"
        ));
    }
    let output_size = options.output_size.unwrap_or(doc.size);
    let set = ExportSetSnapshot {
        id: doc.active_set_id,
        name: doc.active_set_name.clone(),
        source_size: doc.size,
        output_size,
    };
    let composite = |channel| resample_export_image(channel, doc.composite(channel), output_size);
    let count = if options.padding_radius == 0 {
        write_maps(
            output_size,
            dir,
            options.profile.normal_y(options.normal_y),
            composite,
        )?
    } else {
        let coverage = UvCoverage::generate_for_set(
            &doc.mesh,
            output_size,
            &doc.material_bindings,
            doc.active_set_id,
        )?;
        let nearest = nearest_covered_texels(&coverage);
        write_padded_maps(
            output_size,
            dir,
            options.padding_radius,
            options.profile.normal_y(options.normal_y),
            &coverage,
            &nearest,
            composite,
        )?
    };
    write_profile_manifest(options.profile, dir, &set)?;
    Ok(count)
}

/// Exports each set into a distinct numbered folder; titles are sanitized for
/// filesystem safety and IDs prevent collisions between identical names.
pub fn export_all_texture_sets(doc: &Document, dir: &Path) -> Result<usize> {
    export_all_texture_sets_with_options(doc, dir, ExportOptions::default())
}

/// Exports each texture set into its own directory, padding map colors from
/// texels covered by that set's material-filtered UV coverage.
///
/// The radius must be at most [`MAX_PADDING_RADIUS`]. A zero radius uses the
/// legacy export path byte-for-byte. Padding uses nearest Euclidean distance;
/// equal-distance texels select the covered texel with the lower linear index.
pub fn export_all_texture_sets_with_options(
    doc: &Document,
    dir: &Path,
    options: ExportOptions,
) -> Result<usize> {
    validate_output_size(options.output_size)?;
    if options.padding_radius > MAX_PADDING_RADIUS {
        return Err(anyhow!(
            "UV padding radius must be at most {MAX_PADDING_RADIUS} texels"
        ));
    }
    doc.validate()?;
    let mut count = 0;
    for (id, name, size, _) in doc.texture_set_list() {
        let directory = dir.join(set_folder_name(id, &name));
        let output_size = options.output_size.unwrap_or(size);
        let set = ExportSetSnapshot {
            id,
            name,
            source_size: size,
            output_size,
        };
        let composite = |channel| {
            let image = doc
                .composite_for_set(id, channel)
                .ok_or_else(|| anyhow!("Unknown texture set {id}"))?;
            resample_export_image(channel, image, output_size)
        };
        count += if options.padding_radius == 0 {
            write_maps(
                output_size,
                &directory,
                options.profile.normal_y(options.normal_y),
                composite,
            )?
        } else {
            let coverage =
                UvCoverage::generate_for_set(&doc.mesh, output_size, &doc.material_bindings, id)?;
            let nearest = nearest_covered_texels(&coverage);
            write_padded_maps(
                output_size,
                &directory,
                options.padding_radius,
                options.profile.normal_y(options.normal_y),
                &coverage,
                &nearest,
                composite,
            )?
        };
        write_profile_manifest(options.profile, &directory, &set)?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        layer_tree::{FillLayer, LayerGroup, LayerNode, LayerNodeProperties},
        mesh::{Mesh, Vertex},
        uv::UvCoverage,
    };
    use image::Luma;
    use std::collections::HashMap;

    fn output_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kalium-export-{label}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn normal_export_convention_inverts_only_green_and_preserves_default_bytes() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(12, 13, Rgba([72, 111, 224, 173]));
        let expected = doc.composite(Channel::Normal).get_pixel(12, 13).0;
        let opengl_dir = output_dir("normal-opengl");
        let directx_dir = output_dir("normal-directx");
        let all_sets_dir = output_dir("normal-directx-all");

        export_textures(&doc, &opengl_dir).unwrap();
        export_textures_with_options(
            &doc,
            &directx_dir,
            ExportOptions {
                normal_y: NormalYConvention::DirectX,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        export_all_texture_sets_with_options(
            &doc,
            &all_sets_dir,
            ExportOptions {
                normal_y: NormalYConvention::DirectX,
                ..ExportOptions::default()
            },
        )
        .unwrap();

        let opengl = image::open(opengl_dir.join("Kalium_Normal.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(12, 13)
            .0;
        let directx = image::open(directx_dir.join("Kalium_Normal.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(12, 13)
            .0;
        let all_sets = image::open(all_sets_dir.join("001_Default/Kalium_Normal.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(12, 13)
            .0;
        assert_eq!(opengl, expected);
        assert_eq!(
            directx,
            [expected[0], 255 - expected[1], expected[2], expected[3]]
        );
        assert_eq!(all_sets, directx);

        let _ = std::fs::remove_dir_all(opengl_dir);
        let _ = std::fs::remove_dir_all(directx_dir);
        let _ = std::fs::remove_dir_all(all_sets_dir);
    }

    fn two_material_triangles() -> Mesh {
        let vertex = |uv: [f32; 2]| Vertex {
            position: [uv[0], uv[1], 0.0],
            normal: [0.0, 0.0, 1.0],
            uv,
            tangent: None,
            extensions: Default::default(),
        };
        Mesh {
            name: "Two separated UV islands".into(),
            vertices: vec![
                vertex([0.10, 0.10]),
                vertex([0.30, 0.10]),
                vertex([0.10, 0.30]),
                vertex([0.65, 0.65]),
                vertex([0.85, 0.65]),
                vertex([0.65, 0.85]),
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
            materials: vec!["First".into(), "Second".into()],
            triangle_materials: vec![0, 1],
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        }
    }

    fn fill_base_color(doc: &mut Document, color: [u8; 4]) {
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, size)
            .pixels_mut()
            .for_each(|pixel| *pixel = Rgba(color));
    }

    fn color_coverage(doc: &mut Document, set_id: u32, color: [u8; 4]) {
        let size = doc.size;
        let coverage =
            UvCoverage::generate_for_set(&doc.mesh, size, &doc.material_bindings, set_id).unwrap();
        for y in 0..size {
            for x in 0..size {
                if coverage.contains(x, y) {
                    doc.selected_layer_mut()
                        .unwrap()
                        .pixels_mut(Channel::BaseColor, size)
                        .put_pixel(x, y, Rgba(color));
                }
            }
        }
    }

    #[test]
    fn packed_engine_maps_use_exact_source_channels() {
        let mut doc = Document::new(64);
        let sample = (13, 19);
        let set_sample = |doc: &mut Document, channel, value: [u8; 4]| {
            let size = doc.size;
            doc.selected_layer_mut()
                .unwrap()
                .pixels_mut(channel, size)
                .put_pixel(sample.0, sample.1, Rgba(value));
        };
        set_sample(&mut doc, Channel::Occlusion, [37, 37, 37, 255]);
        set_sample(&mut doc, Channel::Roughness, [83, 83, 83, 255]);
        set_sample(&mut doc, Channel::Metallic, [149, 149, 149, 255]);
        set_sample(&mut doc, Channel::Opacity, [211, 211, 211, 255]);
        // Base Color's layer alpha is unrelated to the exported opacity map.
        set_sample(&mut doc, Channel::BaseColor, [25, 90, 180, 255]);

        let dir = output_dir("packed-channels");
        let count = export_textures(&doc, &dir).unwrap();
        assert_eq!(count, 11);
        let orm = image::open(dir.join("Kalium_ORM_Unreal_Godot.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0)
            .0;
        assert_eq!(orm, [255, 170, 0, 255]);
        let orm = image::open(dir.join("Kalium_ORM_Unreal_Godot.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(sample.0, sample.1)
            .0;
        assert_eq!(orm, [37, 83, 149, 255]);

        let unity = image::open(dir.join("Kalium_MetallicSmoothness_Unity.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(sample.0, sample.1)
            .0;
        assert_eq!(unity, [149, 0, 0, 172]);

        let base_alpha = image::open(dir.join("Kalium_BaseColor_Alpha.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(sample.0, sample.1)
            .0;
        assert_eq!(&base_alpha[..3], &[25, 90, 180]);
        assert_eq!(base_alpha[3], 211);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn nested_group_black_mask_and_fill_roundtrip_to_exact_exported_maps() {
        let mut doc = Document::new(64);
        let set_id = doc.active_set_id;
        let mask_on = (8, 9);
        let group_mask_off = (10, 9);
        let fill_mask_off = (12, 9);
        let paint_color = [17, 39, 61, 255];
        let fill_color = [203, 71, 29, 255];
        let fill_roughness = [73, 73, 73, 255];

        let size = doc.size;
        let base = doc.selected_layer_mut().unwrap();
        base.pixels_mut(Channel::BaseColor, size)
            .pixels_mut()
            .for_each(|pixel| *pixel = Rgba(paint_color));

        let mut group = LayerGroup {
            properties: LayerNodeProperties::new(2, "Masked group", doc.size).unwrap(),
            children: vec![LayerNode::Fill(FillLayer {
                properties: LayerNodeProperties::new(3, "Two-channel fill", doc.size).unwrap(),
                values: HashMap::from([
                    (Channel::BaseColor, fill_color),
                    (Channel::Roughness, fill_roughness),
                ]),
            })],
        };
        group
            .properties
            .mask
            .put_pixel(group_mask_off.0, group_mask_off.1, Luma([0]));
        if let LayerNode::Fill(fill) = &mut group.children[0] {
            fill.properties
                .mask
                .put_pixel(fill_mask_off.0, fill_mask_off.1, Luma([0]));
        }
        doc.layer_tree
            .insert(None, 1, LayerNode::Group(group))
            .unwrap();

        let root = output_dir("nested-mask-fill-project");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let project_path = root.join("nested-mask-fill.kalium");
        let before_export = root.join("before-export");
        let after_export = root.join("after-export");

        export_textures(&doc, &before_export).unwrap();
        crate::project::save(&doc, &project_path).unwrap();

        let manifest_text = {
            use std::io::Read;
            let mut archive =
                zip::ZipArchive::new(std::fs::File::open(&project_path).unwrap()).unwrap();
            let mut manifest = String::new();
            archive
                .by_name("manifest.json")
                .unwrap()
                .read_to_string(&mut manifest)
                .unwrap();
            manifest
        };
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["version"], 9);

        let reopened = crate::project::load(&project_path).unwrap();
        assert_eq!(reopened.active_set_id, set_id);
        let LayerNode::Group(group) = reopened.layer_tree.find_node(2).unwrap() else {
            panic!("expected persisted Group node");
        };
        assert_eq!(group.properties.name, "Masked group");
        assert_eq!(
            group
                .properties
                .mask
                .get_pixel(group_mask_off.0, group_mask_off.1)
                .0,
            [0]
        );
        let LayerNode::Fill(fill) = &group.children[0] else {
            panic!("expected persisted Fill node");
        };
        assert_eq!(fill.properties.id, 3);
        assert_eq!(fill.properties.name, "Two-channel fill");
        assert_eq!(fill.values.get(&Channel::BaseColor), Some(&fill_color));
        assert_eq!(fill.values.get(&Channel::Roughness), Some(&fill_roughness));
        assert_eq!(
            fill.properties
                .mask
                .get_pixel(fill_mask_off.0, fill_mask_off.1)
                .0,
            [0]
        );

        let base_color = reopened
            .composite_for_set(set_id, Channel::BaseColor)
            .unwrap();
        let roughness = reopened
            .composite_for_set(set_id, Channel::Roughness)
            .unwrap();
        assert_eq!(base_color.get_pixel(mask_on.0, mask_on.1).0, fill_color);
        assert_eq!(
            base_color.get_pixel(group_mask_off.0, group_mask_off.1).0,
            paint_color
        );
        assert_eq!(
            base_color.get_pixel(fill_mask_off.0, fill_mask_off.1).0,
            paint_color
        );
        assert_eq!(roughness.get_pixel(mask_on.0, mask_on.1).0, fill_roughness);
        assert_eq!(
            roughness.get_pixel(group_mask_off.0, group_mask_off.1).0,
            Channel::Roughness.background()
        );
        assert_eq!(
            roughness.get_pixel(fill_mask_off.0, fill_mask_off.1).0,
            Channel::Roughness.background()
        );

        assert_eq!(export_textures(&reopened, &after_export).unwrap(), 11);
        for (filename, expected) in [
            ("Kalium_BaseColor.png", &base_color),
            ("Kalium_Roughness.png", &roughness),
        ] {
            let before_bytes = std::fs::read(before_export.join(filename)).unwrap();
            let after_bytes = std::fs::read(after_export.join(filename)).unwrap();
            assert_eq!(before_bytes, after_bytes, "reopen changed {filename} bytes");
            let exported = image::load_from_memory(&after_bytes).unwrap().to_rgba8();
            for (point, value) in [
                (mask_on, expected.get_pixel(mask_on.0, mask_on.1).0),
                (
                    group_mask_off,
                    expected.get_pixel(group_mask_off.0, group_mask_off.1).0,
                ),
                (
                    fill_mask_off,
                    expected.get_pixel(fill_mask_off.0, fill_mask_off.1).0,
                ),
            ] {
                assert_eq!(
                    exported.get_pixel(point.0, point.1).0,
                    value,
                    "{filename} at {point:?}"
                );
            }
        }

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn godot_profile_writes_exact_orm_manifest_and_opengl_normal_per_set() {
        let mut doc = Document::new(64);
        let second_set = doc.add_texture_set("Trim").unwrap();
        let sample = (7, 11);
        let size = doc.size;
        for (channel, value) in [
            (Channel::Occlusion, 37),
            (Channel::Roughness, 83),
            (Channel::Metallic, 149),
        ] {
            let layer = doc.selected_layer_mut().unwrap();
            layer.pixels_mut(channel, size).put_pixel(
                sample.0,
                sample.1,
                Rgba([value, value, value, 255]),
            );
        }
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, size)
            .put_pixel(sample.0, sample.1, Rgba([61, 109, 227, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, size)
            .put_pixel(sample.0, sample.1, Rgba([25, 90, 180, 255]));
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Opacity, size)
            .put_pixel(sample.0, sample.1, Rgba([211, 211, 211, 255]));
        let expected_normal = doc
            .composite(Channel::Normal)
            .get_pixel(sample.0, sample.1)
            .0;

        let dir = output_dir("godot-profile");
        std::fs::remove_dir_all(&dir).ok();
        let count = export_all_texture_sets_with_options(
            &doc,
            &dir,
            ExportOptions {
                normal_y: NormalYConvention::DirectX,
                profile: EngineExportProfile::Godot4Orm,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        assert_eq!(count, 22);

        let folder = set_folder_name(second_set, "Trim");
        let set_dir = dir.join(&folder);
        let orm = image::open(set_dir.join("Kalium_ORM_Unreal_Godot.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(orm.get_pixel(sample.0, sample.1).0, [37, 83, 149, 255]);
        let normal = image::open(set_dir.join("Kalium_Normal.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(normal.get_pixel(sample.0, sample.1).0, expected_normal);

        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(set_dir.join("Kalium_Godot4_Profile.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["target"], "Godot 4");
        assert_eq!(manifest["material_type"], "ORMMaterial3D");
        assert_eq!(manifest["texture_set_id"], second_set);
        assert_eq!(manifest["resolution"], serde_json::json!([64, 64]));
        assert_eq!(manifest["orm_channels"]["red"], "occlusion");
        assert_eq!(manifest["orm_channels"]["green"], "roughness");
        assert_eq!(manifest["orm_channels"]["blue"], "metallic");
        assert_eq!(manifest["normal_map_convention"], "OpenGL (+Y)");
        assert_eq!(manifest["textures"]["orm"], "Kalium_ORM_Unreal_Godot.png");

        let unity = output_dir("unity-profile");
        std::fs::remove_dir_all(&unity).ok();
        export_all_texture_sets_with_options(
            &doc,
            &unity,
            ExportOptions {
                profile: EngineExportProfile::UnityMetallic,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        let unity_dir = unity.join("002_Trim");
        let packed = image::open(unity_dir.join("Kalium_MetallicSmoothness_Unity.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(packed.get_pixel(sample.0, sample.1).0, [149, 0, 0, 172]);
        let unity_manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(unity_dir.join("Kalium_Unity_Profile.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(unity_manifest["target"], "Unity");
        assert_eq!(unity_manifest["material_workflow"], "metallic");
        assert_eq!(
            unity_manifest["metallic_smoothness_channels"]["red"],
            "metallic"
        );
        assert_eq!(
            unity_manifest["metallic_smoothness_channels"]["alpha"],
            "smoothness (1 - roughness)"
        );
        assert_eq!(unity_manifest["normal_map_convention"], "OpenGL (+Y)");

        let gltf = output_dir("gltf-profile");
        std::fs::remove_dir_all(&gltf).ok();
        export_all_texture_sets_with_options(
            &doc,
            &gltf,
            ExportOptions {
                normal_y: NormalYConvention::DirectX,
                profile: EngineExportProfile::GltfMetallicRoughness,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        let gltf_dir = gltf.join("002_Trim");
        let gltf_orm = image::open(gltf_dir.join("Kalium_ORM_Unreal_Godot.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(gltf_orm.get_pixel(sample.0, sample.1).0, [37, 83, 149, 255]);
        let gltf_normal = image::open(gltf_dir.join("Kalium_Normal.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(gltf_normal.get_pixel(sample.0, sample.1).0, expected_normal);
        let gltf_manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(gltf_dir.join("Kalium_glTF_Profile.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(gltf_manifest["target"], "glTF 2.0");
        assert_eq!(gltf_manifest["material_workflow"], "metallic-roughness");
        assert_eq!(
            gltf_manifest["textures"]["base_color"],
            "Kalium_BaseColor_Alpha.png"
        );
        assert_eq!(gltf_manifest["texture_set_id"], second_set);
        assert_eq!(gltf_manifest["resolution"], serde_json::json!([64, 64]));
        assert_eq!(
            gltf_manifest["metallic_roughness_channels"]["green"],
            "roughness"
        );
        assert_eq!(
            gltf_manifest["metallic_roughness_channels"]["blue"],
            "metallic"
        );
        assert_eq!(gltf_manifest["occlusion_channel"], "red");
        assert_eq!(gltf_manifest["normal_map_convention"], "OpenGL (+Y)");
        assert_eq!(
            gltf_manifest["textures"]["metallic_roughness"],
            gltf_manifest["textures"]["occlusion"]
        );
        let gltf_base_color = image::open(gltf_dir.join("Kalium_BaseColor_Alpha.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(
            gltf_base_color.get_pixel(sample.0, sample.1).0,
            [25, 90, 180, 211]
        );

        let stepped = output_dir("godot-profile-stepped");
        std::fs::remove_dir_all(&stepped).ok();
        let mut job = ExportJob::new(
            &doc,
            &stepped,
            ExportOptions {
                profile: EngineExportProfile::Godot4Orm,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        loop {
            let progress = job.step(&doc).unwrap();
            if progress.state == ExportJobState::Complete {
                break;
            }
        }
        assert!(stepped
            .join("002_Trim/Kalium_Godot4_Profile.json")
            .is_file());
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(unity).ok();
        std::fs::remove_dir_all(gltf).ok();
        std::fs::remove_dir_all(stepped).ok();
    }

    #[test]
    fn painter_profile_writes_documented_python_export_preset_payload() {
        let doc = Document::new(64);
        let dir = output_dir("painter-preset");
        std::fs::remove_dir_all(&dir).ok();
        let count = export_all_texture_sets_with_options(
            &doc,
            &dir,
            ExportOptions {
                profile: EngineExportProfile::SubstancePainterPython,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        assert_eq!(count, 11);
        let set_dir = dir.join("001_Default");
        let preset: serde_json::Value = serde_json::from_slice(
            &std::fs::read(set_dir.join("Kalium_SubstancePainter_ExportPreset.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(preset["name"], "Kalium PBR texture maps");
        assert_eq!(preset["maps"].as_array().unwrap().len(), 8);
        let base_color = &preset["maps"][0];
        assert_eq!(base_color["fileName"], "$mesh_$textureSet_BaseColor");
        assert_eq!(base_color["parameters"]["fileFormat"], "png");
        assert_eq!(base_color["parameters"]["bitDepth"], "8");
        assert_eq!(base_color["channels"][0]["srcMapName"], "basecolor");
        let occlusion = &preset["maps"][7];
        assert_eq!(occlusion["channels"][0]["srcMapName"], "ambientOcclusion");
        assert_eq!(occlusion["channels"][0]["destChannel"], "L");
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn each_texture_set_exports_to_separate_folder() {
        let mut doc = Document::new(64);
        doc.add_texture_set("Armor / Main").unwrap();
        let dir =
            std::env::temp_dir().join(format!("kalium-export-sets-test-{}", std::process::id()));
        let count = export_all_texture_sets(&doc, &dir).unwrap();
        assert_eq!(count, 22);
        assert!(dir.join("001_Default/Kalium_BaseColor.png").exists());
        assert!(dir
            .join("002_Armor___Main/Kalium_ORM_Unreal_Godot.png")
            .exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn zero_padding_matches_legacy_texture_set_output_byte_for_byte() {
        let mut doc = Document::new(64);
        doc.add_texture_set("Second").unwrap();
        let old = output_dir("zero-legacy");
        let new = output_dir("zero-options");
        let old_count = export_all_texture_sets(&doc, &old).unwrap();
        let new_count = export_all_texture_sets_with_options(
            &doc,
            &new,
            ExportOptions {
                padding_radius: 0,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        assert_eq!(old_count, new_count);
        for (_, name, _, _) in doc.texture_set_list() {
            let safe: String = name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .take(48)
                .collect();
            let folder = format!("{:03}_{safe}", if name == "Default" { 1 } else { 2 });
            for filename in [
                "Kalium_BaseColor.png",
                "Kalium_Roughness.png",
                "Kalium_Metallic.png",
                "Kalium_Normal.png",
                "Kalium_Height.png",
                "Kalium_Emissive.png",
                "Kalium_Opacity.png",
                "Kalium_AmbientOcclusion.png",
                "Kalium_BaseColor_Alpha.png",
                "Kalium_ORM_Unreal_Godot.png",
                "Kalium_MetallicSmoothness_Unity.png",
            ] {
                assert_eq!(
                    std::fs::read(old.join(&folder).join(filename)).unwrap(),
                    std::fs::read(new.join(&folder).join(filename)).unwrap(),
                    "zero padding changed {folder}/{filename}"
                );
            }
        }
        std::fs::remove_dir_all(old).ok();
        std::fs::remove_dir_all(new).ok();
    }

    #[test]
    fn padding_copies_the_nearest_covered_edge_color() {
        let mut doc = Document::new(64);
        doc.import_mesh(two_material_triangles()).unwrap();
        fill_base_color(&mut doc, [12, 22, 32, 255]);
        let active_set_id = doc.active_set_id;
        color_coverage(&mut doc, active_set_id, [231, 41, 17, 255]);
        let coverage = UvCoverage::generate_for_set(
            &doc.mesh,
            doc.size,
            &doc.material_bindings,
            doc.active_set_id,
        )
        .unwrap();
        let nearest = nearest_covered_texels(&coverage);
        let target = (0..doc.size)
            .flat_map(|y| (0..doc.size).map(move |x| (x, y)))
            .find(|&(x, y)| {
                !coverage.contains(x, y)
                    && nearest[y as usize * doc.size as usize + x as usize] != u32::MAX
                    && {
                        let source = nearest[y as usize * doc.size as usize + x as usize] as usize;
                        let dx = (source % doc.size as usize).abs_diff(x as usize) as u32;
                        let dy = (source / doc.size as usize).abs_diff(y as usize) as u32;
                        dx * dx + dy * dy <= 4
                    }
            })
            .unwrap();
        let plain = output_dir("edge-plain");
        let padded = output_dir("edge-padded");
        export_all_texture_sets(&doc, &plain).unwrap();
        export_all_texture_sets_with_options(
            &doc,
            &padded,
            ExportOptions {
                padding_radius: 2,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        let plain_pixel = image::open(plain.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(target.0, target.1)
            .0;
        let padded_pixel = image::open(padded.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(target.0, target.1)
            .0;
        assert_eq!(plain_pixel, [12, 22, 32, 255]);
        assert_eq!(padded_pixel, [231, 41, 17, 255]);
        std::fs::remove_dir_all(plain).ok();
        std::fs::remove_dir_all(padded).ok();
    }

    #[test]
    fn nearest_source_map_is_euclidean_and_uses_stable_ties() {
        let mesh = two_material_triangles();
        let coverage = UvCoverage::generate_for_set(&mesh, 64, &[None, Some(2)], 1).unwrap();
        let nearest = nearest_covered_texels(&coverage);
        let covered: Vec<usize> = (0..64usize * 64)
            .filter(|&index| coverage.contains((index % 64) as u32, (index / 64) as u32))
            .collect();
        assert!(!covered.is_empty());
        for y in 0..64usize {
            for x in 0..64usize {
                let target = y * 64 + x;
                let expected = covered
                    .iter()
                    .copied()
                    .min_by_key(|&source| {
                        let dx = (source % 64).abs_diff(x);
                        let dy = (source / 64).abs_diff(y);
                        (dx * dx + dy * dy, source)
                    })
                    .unwrap() as u32;
                assert_eq!(nearest[target], expected, "nearest source at ({x},{y})");
            }
        }
    }

    #[test]
    fn texture_set_padding_uses_only_its_own_material_coverage() {
        let mut doc = Document::new(64);
        doc.import_mesh(two_material_triangles()).unwrap();
        let second_id = doc.add_texture_set("Second").unwrap();
        doc.bind_material(1, Some(second_id)).unwrap();
        doc.activate_texture_set(1).unwrap();
        fill_base_color(&mut doc, [15, 230, 45, 255]);
        let active_set_id = doc.active_set_id;
        color_coverage(&mut doc, active_set_id, [240, 20, 10, 255]);
        doc.activate_texture_set(second_id).unwrap();
        fill_base_color(&mut doc, [20, 40, 230, 255]);
        color_coverage(&mut doc, second_id, [20, 40, 230, 255]);
        doc.activate_texture_set(1).unwrap();

        let dir = output_dir("set-isolation");
        export_all_texture_sets_with_options(
            &doc,
            &dir,
            ExportOptions {
                padding_radius: 4,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        let first = image::open(dir.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        let second = image::open(dir.join("002_Second/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        // This texel is outside the second island but within its padding radius.
        let foreign_island_pixel = (39, 42);
        assert_eq!(
            first
                .get_pixel(foreign_island_pixel.0, foreign_island_pixel.1)
                .0,
            [15, 230, 45, 255]
        );
        assert_eq!(
            second
                .get_pixel(foreign_island_pixel.0, foreign_island_pixel.1)
                .0,
            [20, 40, 230, 255]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    fn png_files_under(root: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(png_files_under(&path));
            } else if path.extension().is_some_and(|extension| extension == "png") {
                files.push(path);
            }
        }
        files.sort();
        files
    }

    #[test]
    fn export_job_matches_synchronous_all_set_bytes_with_padding_and_directx_normals() {
        let mut doc = Document::new(64);
        doc.import_mesh(two_material_triangles()).unwrap();
        let second_set = doc.add_texture_set("Second set").unwrap();
        doc.bind_material(1, Some(second_set)).unwrap();
        fill_base_color(&mut doc, [13, 27, 39, 255]);
        let first_set = doc.active_set_id;
        color_coverage(&mut doc, first_set, [201, 61, 23, 255]);
        doc.activate_texture_set(second_set).unwrap();
        fill_base_color(&mut doc, [7, 19, 31, 255]);
        color_coverage(&mut doc, second_set, [42, 174, 93, 255]);
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, size)
            .put_pixel(42, 42, Rgba([62, 71, 223, 255]));
        doc.activate_texture_set(1).unwrap();

        let synchronous = output_dir("job-parity-sync");
        let stepped = output_dir("job-parity-stepped");
        std::fs::remove_dir_all(&synchronous).ok();
        std::fs::remove_dir_all(&stepped).ok();
        let options = ExportOptions {
            output_size: None,
            padding_radius: 3,
            normal_y: NormalYConvention::DirectX,
            profile: EngineExportProfile::Custom,
        };
        let sync_count = export_all_texture_sets_with_options(&doc, &synchronous, options).unwrap();
        let mut job = ExportJob::new(&doc, &stepped, options).unwrap();
        let total = doc.texture_set_count() * MAP_FILENAMES.len();
        let mut progress = None;
        for step_index in 0..total {
            let before = png_files_under(&stepped).len();
            let next = job.step(&doc).unwrap();
            assert_eq!(png_files_under(&stepped).len(), before + 1);
            assert_eq!(next.completed_maps, step_index + 1);
            let map = next.current.as_ref().unwrap();
            assert_eq!(map.set_index, step_index / MAP_FILENAMES.len() + 1);
            assert_eq!(map.map_index, step_index % MAP_FILENAMES.len() + 1);
            assert_eq!(map.map_count, MAP_FILENAMES.len());
            assert_eq!(
                map.map_filename,
                MAP_FILENAMES[step_index % MAP_FILENAMES.len()]
            );
            assert_eq!(
                next.state,
                if step_index + 1 == total {
                    ExportJobState::Complete
                } else {
                    ExportJobState::Running
                }
            );
            progress = Some(next);
        }
        assert_eq!(sync_count, total);
        assert_eq!(progress.unwrap().total_maps, total);
        assert_eq!(png_files_under(&synchronous).len(), total);
        assert_eq!(png_files_under(&stepped).len(), total);
        for sync_file in png_files_under(&synchronous) {
            let relative = sync_file.strip_prefix(&synchronous).unwrap();
            assert_eq!(
                std::fs::read(&sync_file).unwrap(),
                std::fs::read(stepped.join(relative)).unwrap(),
                "stepped export changed {}",
                relative.display()
            );
        }
        assert_eq!(
            job.step(&doc).unwrap().state,
            ExportJobState::Complete,
            "stepping a completed job should remain complete"
        );
        std::fs::remove_dir_all(synchronous).ok();
        std::fs::remove_dir_all(stepped).ok();
    }

    #[test]
    fn canceling_export_job_reports_cancelled_and_keeps_only_partial_output() {
        let mut doc = Document::new(64);
        doc.add_texture_set("Second").unwrap();
        let output = output_dir("job-cancel");
        std::fs::remove_dir_all(&output).ok();
        let mut job = ExportJob::new(
            &doc,
            &output,
            ExportOptions {
                padding_radius: 2,
                ..ExportOptions::default()
            },
        )
        .unwrap();
        let first = job.step(&doc).unwrap();
        let second = job.step(&doc).unwrap();
        assert_eq!(first.state, ExportJobState::Running);
        assert_eq!(first.completed_maps, 1);
        assert_eq!(second.state, ExportJobState::Running);
        assert_eq!(second.completed_maps, 2);
        assert_eq!(png_files_under(&output).len(), 2);

        job.cancel();
        let cancelled = job.step(&doc).unwrap();
        assert_eq!(cancelled.state, ExportJobState::Cancelled);
        assert_eq!(cancelled.completed_maps, 2);
        assert_eq!(cancelled.total_maps, 22);
        assert!(cancelled.current.is_none());
        assert_eq!(png_files_under(&output).len(), 2);
        assert_eq!(job.step(&doc).unwrap().state, ExportJobState::Cancelled);
        std::fs::remove_dir_all(output).ok();
    }

    #[test]
    fn active_export_job_matches_legacy_flat_output_bytes() {
        let mut doc = Document::new(64);
        let active_set = doc.add_texture_set("Active / Set").unwrap();
        doc.activate_texture_set(active_set).unwrap();
        fill_base_color(&mut doc, [39, 81, 143, 255]);
        let size = doc.size;
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, size)
            .put_pixel(17, 23, Rgba([80, 111, 219, 255]));

        let synchronous = output_dir("job-active-sync");
        let stepped = output_dir("job-active-stepped");
        std::fs::remove_dir_all(&synchronous).ok();
        std::fs::remove_dir_all(&stepped).ok();
        let options = ExportOptions {
            output_size: None,
            padding_radius: 2,
            normal_y: NormalYConvention::DirectX,
            profile: EngineExportProfile::Custom,
        };
        assert_eq!(
            export_textures_with_options(&doc, &synchronous, options).unwrap(),
            11
        );
        let mut job = ExportJob::new_active(&doc, &stepped, options).unwrap();
        for map_index in 0..MAP_FILENAMES.len() {
            let progress = job.step(&doc).unwrap();
            assert_eq!(progress.completed_maps, map_index + 1);
            assert_eq!(progress.total_maps, MAP_FILENAMES.len());
            assert_eq!(progress.current.as_ref().unwrap().set_id, active_set);
            assert_eq!(progress.current.as_ref().unwrap().set_index, 1);
            assert_eq!(progress.current.as_ref().unwrap().set_count, 1);
            assert_eq!(progress.current.as_ref().unwrap().map_index, map_index + 1);
            assert_eq!(
                progress.state,
                if map_index + 1 == MAP_FILENAMES.len() {
                    ExportJobState::Complete
                } else {
                    ExportJobState::Running
                }
            );
            assert_eq!(png_files_under(&stepped).len(), map_index + 1);
        }
        for filename in MAP_FILENAMES {
            assert_eq!(
                std::fs::read(synchronous.join(filename)).unwrap(),
                std::fs::read(stepped.join(filename)).unwrap(),
                "active stepped export changed {filename}"
            );
        }
        assert_eq!(job.step(&doc).unwrap().state, ExportJobState::Complete);
        std::fs::remove_dir_all(synchronous).ok();
        std::fs::remove_dir_all(stepped).ok();
    }

    #[test]
    fn explicit_export_resolution_resamples_all_maps_and_keeps_document_unchanged() {
        let mut doc = Document::new(64);
        doc.add_texture_set("Trim").unwrap();
        let before_set_list = doc.texture_set_list();
        let before_bytes = doc.decoded_bytes();
        let before_composites: Vec<_> = Channel::ALL
            .into_iter()
            .map(|channel| doc.composite(channel))
            .collect();
        let dir = output_dir("output-resolution-all-maps");
        std::fs::remove_dir_all(&dir).ok();

        let written = export_all_texture_sets_with_options(
            &doc,
            &dir,
            ExportOptions {
                output_size: Some(128),
                profile: EngineExportProfile::Godot4Orm,
                ..ExportOptions::default()
            },
        )
        .unwrap();

        assert_eq!(written, 22);
        for folder in ["001_Default", "002_Trim"] {
            for filename in MAP_FILENAMES {
                let image = image::open(dir.join(folder).join(filename))
                    .unwrap()
                    .to_rgba8();
                assert_eq!(image.dimensions(), (128, 128), "{folder}/{filename}");
            }
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(dir.join(folder).join("Kalium_Godot4_Profile.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["resolution"], serde_json::json!([128, 128]));
        }
        assert_eq!(doc.size, 64);
        assert_eq!(doc.decoded_bytes(), before_bytes);
        assert_eq!(doc.texture_set_list(), before_set_list);
        for (channel, before) in Channel::ALL.into_iter().zip(before_composites) {
            assert_eq!(doc.composite(channel), before);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resampling_uses_normal_vectors_and_scalar_data_filtering() {
        let mut normals = RgbaImage::new(2, 2);
        for y in 0..2 {
            normals.put_pixel(0, y, Rgba([128, 128, 255, 255]));
            normals.put_pixel(1, y, Rgba([255, 128, 128, 255]));
        }
        let resized_normal = resample_export_image(Channel::Normal, normals, 4).unwrap();
        let normal = resized_normal.get_pixel(1, 1).0;
        assert!(normal[0] >= 166 && normal[0] <= 170, "{normal:?}");
        assert!(normal[2] >= 246 && normal[2] <= 250, "{normal:?}");
        assert_eq!(normal[1], 128);

        let mut scalar = RgbaImage::new(2, 2);
        for y in 0..2 {
            scalar.put_pixel(0, y, Rgba([0, 0, 0, 255]));
            scalar.put_pixel(1, y, Rgba([255, 255, 255, 255]));
        }
        let resized_scalar = resample_export_image(Channel::Roughness, scalar, 4).unwrap();
        let value = resized_scalar.get_pixel(1, 1).0;
        assert!((63..=65).contains(&value[0]), "{value:?}");
        assert_eq!(value[0], value[1]);
        assert_eq!(value[1], value[2]);

        let mut color = RgbaImage::new(2, 2);
        for y in 0..2 {
            color.put_pixel(0, y, Rgba([0, 0, 0, 255]));
            color.put_pixel(1, y, Rgba([255, 255, 255, 255]));
        }
        let resized_color = resample_export_image(Channel::BaseColor, color, 3).unwrap();
        let midpoint = resized_color.get_pixel(1, 1).0;
        assert!((187..=189).contains(&midpoint[0]), "{midpoint:?}");
    }

    #[test]
    fn output_resolution_padding_uses_output_texel_coverage_and_radius() {
        let mut doc = Document::new(64);
        doc.import_mesh(two_material_triangles()).unwrap();
        fill_base_color(&mut doc, [12, 22, 32, 255]);
        let active_set_id = doc.active_set_id;
        color_coverage(&mut doc, active_set_id, [31, 61, 91, 255]);
        let output_size = 128;
        let coverage = UvCoverage::generate_for_set(
            &doc.mesh,
            output_size,
            &doc.material_bindings,
            doc.active_set_id,
        )
        .unwrap();
        let nearest = nearest_covered_texels(&coverage);
        let plain = output_dir("output-resolution-padding-plain");
        let padded = output_dir("output-resolution-padding-padded");
        std::fs::remove_dir_all(&plain).ok();
        std::fs::remove_dir_all(&padded).ok();
        for (dir, padding_radius) in [(&plain, 0), (&padded, 2)] {
            export_all_texture_sets_with_options(
                &doc,
                dir,
                ExportOptions {
                    output_size: Some(output_size),
                    padding_radius,
                    ..ExportOptions::default()
                },
            )
            .unwrap();
        }
        let plain_image = image::open(plain.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        let padded_image = image::open(padded.join("001_Default/Kalium_BaseColor.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(plain_image.dimensions(), (output_size, output_size));
        let target = (0..output_size)
            .flat_map(|y| (0..output_size).map(move |x| (x, y)))
            .find(|&(x, y)| {
                let index = y as usize * output_size as usize + x as usize;
                if coverage.contains(x, y) || nearest[index] == u32::MAX {
                    return false;
                }
                let source = nearest[index] as usize;
                let dx = (source % output_size as usize).abs_diff(x as usize) as u32;
                let dy = (source / output_size as usize).abs_diff(y as usize) as u32;
                dx * dx + dy * dy <= 4
                    && plain_image.get_pixel(x, y)
                        != plain_image.get_pixel(
                            (source % output_size as usize) as u32,
                            (source / output_size as usize) as u32,
                        )
            })
            .unwrap();
        let source = nearest[target.1 as usize * output_size as usize + target.0 as usize] as usize;
        assert_eq!(
            padded_image.get_pixel(target.0, target.1),
            plain_image.get_pixel(
                (source % output_size as usize) as u32,
                (source / output_size as usize) as u32,
            )
        );
        std::fs::remove_dir_all(plain).ok();
        std::fs::remove_dir_all(padded).ok();
    }

    #[test]
    fn export_resolution_rejects_unsupported_size() {
        let doc = Document::new(64);
        let result = ExportJob::new(
            &doc,
            output_dir("unsupported-resolution"),
            ExportOptions {
                output_size: Some(65),
                ..ExportOptions::default()
            },
        );
        let error = match result {
            Ok(_) => panic!("unsupported export resolution was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Export resolution"));
    }
}

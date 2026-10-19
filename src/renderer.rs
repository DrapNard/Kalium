//! Deterministic reference viewport. The desktop UI runs on wgpu; this
//! software rasterizer provides predictable UV hit-testing for the first alpha.
//! A native wgpu mesh/compute path is a planned milestone, not implemented here.
use crate::document::{Channel, Document};
use crate::mesh::Mesh;
use glam::{Vec2, Vec3};
use image::RgbaImage;
use std::collections::{HashMap, HashSet};

const MAX_REFRESH_TILE_CELLS: usize = 1_048_576;
const MAX_VIEWPORT_WIDTH: usize = 1100;
const MAX_VIEWPORT_HEIGHT: usize = 900;
const MAX_VIEWPORT_PIXELS: usize = MAX_VIEWPORT_WIDTH * MAX_VIEWPORT_HEIGHT;
const SCREEN_DAMAGE_CELL_SIZE: usize = 16;
const MAX_SCREEN_DAMAGE_RECTS: usize = 64;
/// Standard PBR compositing retains at most this many nearest surfaces per
/// pixel when transparency is present. Deeper surfaces are omitted from the
/// viewport composite; picking still reports the nearest geometric surface.
const MAX_COMPOSITING_LAYERS_PER_PIXEL: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PickHit {
    pub uv: [f32; 2],
    pub material_slot: usize,
    pub triangle_index: usize,
    pub barycentric: [f32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenRefresh {
    CacheInvalid,
    NoVisiblePixels,
    Updated(Vec<ScreenRect>),
}

fn coalesce_screen_damage_cells(
    damaged_cells: &[bool],
    width: usize,
    height: usize,
) -> Option<Vec<ScreenRect>> {
    let columns = width.div_ceil(SCREEN_DAMAGE_CELL_SIZE);
    let rows = height.div_ceil(SCREEN_DAMAGE_CELL_SIZE);
    let cell_count = columns.checked_mul(rows)?;
    if columns == 0 || rows == 0 || damaged_cells.len() != cell_count {
        return None;
    }

    let mut visited = Vec::new();
    visited.try_reserve_exact(cell_count).ok()?;
    visited.resize(cell_count, false);
    let mut pending = Vec::new();
    pending.try_reserve_exact(cell_count).ok()?;
    let mut rects = Vec::new();
    rects.try_reserve_exact(MAX_SCREEN_DAMAGE_RECTS).ok()?;

    for start in 0..cell_count {
        if !damaged_cells[start] || visited[start] {
            continue;
        }
        if rects.len() == MAX_SCREEN_DAMAGE_RECTS {
            return None;
        }

        let mut min_x = start % columns;
        let mut max_x = min_x;
        let mut min_y = start / columns;
        let mut max_y = min_y;
        visited[start] = true;
        pending.push(start);
        while let Some(index) = pending.pop() {
            let x = index % columns;
            let y = index / columns;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);

            let mut visit = |neighbor: usize| {
                if damaged_cells[neighbor] && !visited[neighbor] {
                    visited[neighbor] = true;
                    pending.push(neighbor);
                }
            };
            if x > 0 {
                visit(index - 1);
            }
            if x + 1 < columns {
                visit(index + 1);
            }
            if y > 0 {
                visit(index - columns);
            }
            if y + 1 < rows {
                visit(index + columns);
            }
        }

        let x = min_x * SCREEN_DAMAGE_CELL_SIZE;
        let y = min_y * SCREEN_DAMAGE_CELL_SIZE;
        let right = ((max_x + 1) * SCREEN_DAMAGE_CELL_SIZE).min(width);
        let bottom = ((max_y + 1) * SCREEN_DAMAGE_CELL_SIZE).min(height);
        rects.push(ScreenRect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        });
    }
    Some(rects)
}

#[derive(Clone, Copy)]
struct ScreenSurface {
    pick: PickHit,
    position: Vec3,
    normal: Vec3,
    tangent: Vec3,
    tangent_handedness: f32,
}

#[derive(Clone, Copy)]
struct CompositeFragment {
    depth: f32,
    color: [u8; 3],
    opacity: u8,
}

fn insert_composite_fragment(fragments: &mut Vec<CompositeFragment>, fragment: CompositeFragment) {
    if fragments
        .iter()
        .any(|existing| existing.depth == fragment.depth)
    {
        return;
    }
    let position = fragments
        .binary_search_by(|existing| existing.depth.total_cmp(&fragment.depth))
        .unwrap_or_else(|position| position);
    if position >= MAX_COMPOSITING_LAYERS_PER_PIXEL {
        return;
    }
    fragments.insert(position, fragment);
    if fragments.len() > MAX_COMPOSITING_LAYERS_PER_PIXEL {
        fragments.pop();
    }
}

fn composite_fragments(
    fragments: &[CompositeFragment],
    width: usize,
    height: usize,
    index: usize,
) -> ([u8; 4], bool) {
    let y = index / width;
    let shade = ((y as f32 / height.max(1) as f32) * 14.0).round() as u8;
    let background = [27 + shade / 3, 27 + shade / 3, 36 + shade];
    let mut remaining = 1.0_f32;
    let mut color = Vec3::ZERO;
    let mut visible_transparency = false;
    for fragment in fragments {
        let alpha = fragment.opacity as f32 / 255.0;
        visible_transparency |= alpha < 1.0 && remaining > f32::EPSILON;
        let layer =
            srgb_rgb_to_linear([fragment.color[0], fragment.color[1], fragment.color[2], 255]);
        color += layer * (remaining * alpha);
        remaining *= 1.0 - alpha;
        if remaining <= f32::EPSILON {
            break;
        }
    }
    color += srgb_rgb_to_linear([background[0], background[1], background[2], 255]) * remaining;
    let encoded = linear_rgb_to_srgb(color);
    (
        [encoded[0], encoded[1], encoded[2], 255],
        visible_transparency,
    )
}

#[derive(Clone, Debug, PartialEq)]
struct RenderCacheKey {
    active_set_id: u32,
    bindings: Vec<Option<u32>>,
    solo: Option<Channel>,
    set_dimensions: Vec<(u32, u32, u32)>,
    lighting: PbrLightingSettings,
    visible_transparency: bool,
}

pub struct Material {
    maps: HashMap<Channel, RgbaImage>,
}
impl Material {
    pub fn from_document(doc: &Document) -> Self {
        Self {
            maps: Channel::ALL
                .into_iter()
                .map(|c| (c, doc.composite(c)))
                .collect(),
        }
    }
    pub fn get(&self, ch: Channel, uv: Vec2) -> [u8; 4] {
        let img = &self.maps[&ch];
        let x = (uv.x.clamp(0.0, 1.0) * (img.width() - 1) as f32).round() as u32;
        let y = (uv.y.clamp(0.0, 1.0) * (img.height() - 1) as f32).round() as u32;
        img.get_pixel(x, y).0
    }
    pub fn image(&self, ch: Channel) -> &RgbaImage {
        &self.maps[&ch]
    }

    fn patch_region(&mut self, ch: Channel, x: u32, y: u32, region: &RgbaImage) -> bool {
        let Some(image) = self.maps.get_mut(&ch) else {
            return false;
        };
        let Some(right) = x.checked_add(region.width()) else {
            return false;
        };
        let Some(bottom) = y.checked_add(region.height()) else {
            return false;
        };
        if region.width() == 0
            || region.height() == 0
            || right > image.width()
            || bottom > image.height()
        {
            return false;
        }
        let destination_stride = image.width() as usize * 4;
        let source_stride = region.width() as usize * 4;
        for row in 0..region.height() as usize {
            let destination_start = (y as usize + row) * destination_stride + x as usize * 4;
            let source_start = row * source_stride;
            image.as_mut()[destination_start..destination_start + source_stride]
                .copy_from_slice(&region.as_raw()[source_start..source_start + source_stride]);
        }
        true
    }
}

/// Composited channel maps for each Texture Set. The reference rasterizer uses
/// the same material binding map as the project and the painting picker.
pub struct MaterialLibrary {
    active_set_id: u32,
    sets: HashMap<u32, Material>,
}

impl MaterialLibrary {
    pub fn from_document(doc: &Document) -> Self {
        let mut sets = HashMap::new();
        for (id, _, _, _) in doc.texture_set_list() {
            let maps = Channel::ALL
                .into_iter()
                .filter_map(|channel| {
                    doc.composite_for_set(id, channel)
                        .map(|image| (channel, image))
                })
                .collect();
            sets.insert(id, Material { maps });
        }
        Self {
            active_set_id: doc.active_set_id,
            sets,
        }
    }
    pub fn active(&self) -> &Material {
        // An active document always owns one Texture Set.
        &self.sets[&self.active_set_id]
    }
    pub fn patch_region(
        &mut self,
        set_id: u32,
        channel: Channel,
        x: u32,
        y: u32,
        region: &RgbaImage,
    ) -> bool {
        self.sets
            .get_mut(&set_id)
            .is_some_and(|material| material.patch_region(channel, x, y, region))
    }

    /// Replace one Texture Set's channel map for display-only modes such as
    /// selected-layer isolation. This does not mutate the source Document.
    pub fn replace_channel_map(&mut self, set_id: u32, channel: Channel, image: RgbaImage) -> bool {
        let Some(material) = self.sets.get_mut(&set_id) else {
            return false;
        };
        let Some(current) = material.maps.get(&channel) else {
            return false;
        };
        if image.dimensions() != current.dimensions() {
            return false;
        }
        material.maps.insert(channel, image);
        true
    }
    fn resolve(&self, material_slot: usize, bindings: &[Option<u32>]) -> &Material {
        let id = bindings
            .get(material_slot)
            .copied()
            .flatten()
            .unwrap_or(self.active_set_id);
        self.sets.get(&id).unwrap_or_else(|| self.active())
    }

    fn set_dimensions(&self) -> Vec<(u32, u32, u32)> {
        let mut dimensions = self
            .sets
            .iter()
            .filter_map(|(set_id, material)| {
                let image = material.maps.values().next()?;
                Some((*set_id, image.width(), image.height()))
            })
            .collect::<Vec<_>>();
        dimensions.sort_unstable();
        dimensions
    }
}

/// Adjustable deterministic lighting for the CPU reference viewport.
/// Angles use degrees; azimuth 0 points along +Z and elevation 0 lies on XZ.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EnvironmentPreset {
    /// Neutral blue-gray studio sky, broad horizon, and dark floor.
    #[default]
    Studio,
    /// Warm key-side sky and amber floor bounce.
    Warm,
    /// Cool sky and blue floor bounce.
    Cool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PbrLightingSettings {
    pub key_azimuth_degrees: f32,
    pub key_elevation_degrees: f32,
    pub key_intensity: f32,
    pub ambient_fill: f32,
    /// Selects a built-in analytic environment; no image or external asset is loaded.
    pub environment: EnvironmentPreset,
}

impl Default for PbrLightingSettings {
    fn default() -> Self {
        let direction = Vec3::new(0.3, 0.83, 0.55).normalize();
        Self {
            key_azimuth_degrees: direction.x.atan2(direction.z).to_degrees(),
            key_elevation_degrees: direction.y.asin().to_degrees(),
            key_intensity: 1.8,
            ambient_fill: 0.35,
            environment: EnvironmentPreset::Studio,
        }
    }
}

impl PbrLightingSettings {
    fn validated(self) -> Self {
        let defaults = Self::default();
        let finite_clamp = |value: f32, fallback: f32, min: f32, max: f32| {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                fallback
            }
        };
        Self {
            key_azimuth_degrees: finite_clamp(
                self.key_azimuth_degrees,
                defaults.key_azimuth_degrees,
                -180.0,
                180.0,
            ),
            key_elevation_degrees: finite_clamp(
                self.key_elevation_degrees,
                defaults.key_elevation_degrees,
                -89.0,
                89.0,
            ),
            key_intensity: finite_clamp(self.key_intensity, defaults.key_intensity, 0.0, 4.0),
            ambient_fill: finite_clamp(self.ambient_fill, defaults.ambient_fill, 0.0, 2.0),
            environment: self.environment,
        }
    }

    fn lighting(self) -> PbrLighting {
        let settings = self.validated();
        let azimuth = settings.key_azimuth_degrees.to_radians();
        let elevation = settings.key_elevation_degrees.to_radians();
        let horizontal = elevation.cos();
        PbrLighting {
            direction_to_light: Vec3::new(
                azimuth.sin() * horizontal,
                elevation.sin(),
                azimuth.cos() * horizontal,
            ),
            direct_radiance: Vec3::splat(settings.key_intensity),
            ambient_irradiance: Vec3::splat(settings.ambient_fill),
            environment: Some(settings.environment),
        }
    }
}

fn shade_sample(
    material: &Material,
    surface: &ScreenSurface,
    eye: Vec3,
    solo: Option<Channel>,
    lighting: PbrLighting,
) -> [u8; 3] {
    let uv = Vec2::from_array(surface.pick.uv);
    let normal = if solo.is_none() {
        let tex = material.get(Channel::Normal, uv);
        let tangent_normal = Vec3::new(
            (tex[0] as f32 / 255.0) * 2.0 - 1.0,
            (tex[1] as f32 / 255.0) * 2.0 - 1.0,
            (tex[2] as f32 / 255.0) * 2.0 - 1.0,
        );
        let tangent = (surface.tangent - surface.normal * surface.tangent.dot(surface.normal))
            .normalize_or_zero();
        let bitangent = surface.normal.cross(tangent) * surface.tangent_handedness;
        (tangent * tangent_normal.x
            + bitangent * tangent_normal.y
            + surface.normal * tangent_normal.z)
            .normalize_or_zero()
    } else {
        surface.normal
    };
    let diffuse = material.get(solo.unwrap_or(Channel::BaseColor), uv);
    if solo.is_some() {
        [diffuse[0], diffuse[1], diffuse[2]]
    } else {
        linear_rgb_to_srgb(shade_pbr(
            material_surface(material, uv),
            normal,
            (eye - surface.position).normalize_or_zero(),
            lighting,
        ))
    }
}

#[derive(Clone, Copy)]
struct ScreenVertex {
    xy: Vec2,
    z: f32,
    uv: Vec2,
    n: Vec3,
    tangent: Vec3,
    tangent_handedness: f32,
    p: Vec3,
    barycentric: [f32; 3],
}

#[derive(Clone, Copy)]
struct ClipVertex {
    p: Vec3,
    n: Vec3,
    uv: Vec2,
    tangent: Vec3,
    tangent_handedness: f32,
    depth: f32,
    barycentric: [f32; 3],
}

const NEAR_PLANE: f32 = 0.05;
const PI: f32 = std::f32::consts::PI;
const MIN_PERCEPTUAL_ROUGHNESS: f32 = 0.045;

/// Linear-light surface values for the deterministic CPU reference shader.
#[derive(Clone, Copy, Debug)]
struct PbrSurface {
    base_color: Vec3,
    metallic: f32,
    roughness: f32,
    ao: f32,
    emissive: Vec3,
}

/// One direct directional source, a constant fill, and an optional analytic environment.
#[derive(Clone, Copy, Debug)]
struct PbrLighting {
    direction_to_light: Vec3,
    direct_radiance: Vec3,
    ambient_irradiance: Vec3,
    environment: Option<EnvironmentPreset>,
}

#[derive(Clone, Copy)]
struct AnalyticEnvironment {
    zenith: Vec3,
    horizon: Vec3,
    ground: Vec3,
    key_direction: Vec3,
    key_color: Vec3,
    key_strength: f32,
}

impl AnalyticEnvironment {
    fn from_preset(preset: EnvironmentPreset) -> Self {
        match preset {
            EnvironmentPreset::Studio => Self {
                zenith: Vec3::new(0.32, 0.40, 0.55),
                horizon: Vec3::new(0.72, 0.76, 0.82),
                ground: Vec3::new(0.20, 0.19, 0.18),
                key_direction: Vec3::new(-0.35, 0.72, 0.60).normalize(),
                key_color: Vec3::new(1.0, 0.88, 0.72),
                key_strength: 1.8,
            },
            EnvironmentPreset::Warm => Self {
                zenith: Vec3::new(0.46, 0.34, 0.25),
                horizon: Vec3::new(0.86, 0.65, 0.43),
                ground: Vec3::new(0.31, 0.20, 0.12),
                key_direction: Vec3::new(-0.55, 0.58, 0.60).normalize(),
                key_color: Vec3::new(1.0, 0.61, 0.34),
                key_strength: 2.1,
            },
            EnvironmentPreset::Cool => Self {
                zenith: Vec3::new(0.24, 0.36, 0.62),
                horizon: Vec3::new(0.58, 0.72, 0.90),
                ground: Vec3::new(0.13, 0.20, 0.30),
                key_direction: Vec3::new(0.48, 0.72, 0.50).normalize(),
                key_color: Vec3::new(0.53, 0.73, 1.0),
                key_strength: 1.9,
            },
        }
    }

    fn radiance(self, direction: Vec3, roughness: f32) -> Vec3 {
        let direction = finite_direction(direction);
        let elevation = direction.y.clamp(-1.0, 1.0);
        let sky = self.horizon.lerp(self.zenith, elevation.max(0.0).sqrt());
        let base = if elevation >= 0.0 {
            sky
        } else {
            self.horizon.lerp(self.ground, (-elevation).sqrt())
        };
        // An analytic broad source stands in for a studio softbox. Increasing
        // roughness broadens its angular response and lowers the peak.
        let roughness = roughness.clamp(MIN_PERCEPTUAL_ROUGHNESS, 1.0);
        let angular_blur = 0.015 + roughness * roughness * 0.30;
        let alignment = direction.dot(self.key_direction).clamp(-1.0, 1.0);
        let angular_distance_squared = 2.0 * (1.0 - alignment);
        let lobe = (-angular_distance_squared / angular_blur).exp();
        base + self.key_color * (self.key_strength * lobe / (1.0 + 7.0 * roughness))
    }

    fn diffuse_irradiance(self, normal: Vec3) -> Vec3 {
        let up = finite_direction(normal).y.clamp(-1.0, 1.0);
        let upper = (up + 1.0) * 0.5;
        self.ground.lerp(self.horizon.lerp(self.zenith, 0.5), upper) * PI
    }
}

fn finite_nonnegative(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        fallback
    }
}

fn finite_color(value: Vec3) -> Vec3 {
    Vec3::new(
        finite_nonnegative(value.x, 0.0),
        finite_nonnegative(value.y, 0.0),
        finite_nonnegative(value.z, 0.0),
    )
}

fn finite_direction(value: Vec3) -> Vec3 {
    let length_squared = value.length_squared();
    if value.is_finite() && length_squared.is_finite() && length_squared > 1.0e-12 {
        value / length_squared.sqrt()
    } else {
        Vec3::ZERO
    }
}

fn srgb_channel_to_linear(encoded: u8) -> f32 {
    let channel = encoded as f32 / 255.0;
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_channel_to_srgb(linear: f32) -> u8 {
    let linear = finite_nonnegative(linear, 0.0).clamp(0.0, 1.0);
    let encoded = if linear <= 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn srgb_rgb_to_linear(rgb: [u8; 4]) -> Vec3 {
    Vec3::new(
        srgb_channel_to_linear(rgb[0]),
        srgb_channel_to_linear(rgb[1]),
        srgb_channel_to_linear(rgb[2]),
    )
}

fn linear_rgb_to_srgb(rgb: Vec3) -> [u8; 3] {
    [
        linear_channel_to_srgb(rgb.x),
        linear_channel_to_srgb(rgb.y),
        linear_channel_to_srgb(rgb.z),
    ]
}

fn fresnel_schlick(cos_theta: f32, f0: Vec3) -> Vec3 {
    let factor = (1.0 - cos_theta.clamp(0.0, 1.0)).powi(5);
    f0 + (Vec3::ONE - f0) * factor
}

/// Evaluate a direct-light Cook–Torrance BRDF with GGX distribution and Smith
/// masking, plus Lambertian fill and optional analytic environment lighting.
/// All colors and directions are linear-light; normals and directions point
/// toward the viewer/light.
fn shade_pbr(surface: PbrSurface, normal: Vec3, view: Vec3, lighting: PbrLighting) -> Vec3 {
    let base_color = finite_color(surface.base_color);
    let emissive = finite_color(surface.emissive);
    let metallic = if surface.metallic.is_finite() {
        surface.metallic.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ao = if surface.ao.is_finite() {
        surface.ao.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let roughness = if surface.roughness.is_finite() {
        surface.roughness.clamp(MIN_PERCEPTUAL_ROUGHNESS, 1.0)
    } else {
        1.0
    };
    let n = finite_direction(normal);
    let v = finite_direction(view);
    let l = finite_direction(lighting.direction_to_light);
    let direct_radiance = finite_color(lighting.direct_radiance);
    let ambient_irradiance = finite_color(lighting.ambient_irradiance);
    let ndot_l = n.dot(l).clamp(0.0, 1.0);

    let f0 = Vec3::splat(0.04).lerp(base_color, metallic);
    let ambient = base_color * (1.0 - metallic) * (Vec3::ONE - f0) * ambient_irradiance * (ao / PI);
    let environment = lighting.environment.map(AnalyticEnvironment::from_preset);
    let environment_diffuse = environment.map_or(Vec3::ZERO, |environment| {
        base_color
            * (1.0 - metallic)
            * (Vec3::ONE - f0)
            * environment.diffuse_irradiance(n)
            * (ao / PI)
    });
    let ndot_v = n.dot(v).clamp(0.0, 1.0);

    let mut direct = Vec3::ZERO;
    if ndot_v > 0.0 && ndot_l > 0.0 {
        let h = (v + l).normalize_or_zero();
        let ndot_h = n.dot(h).clamp(0.0, 1.0);
        let vdot_h = v.dot(h).clamp(0.0, 1.0);
        let alpha = roughness * roughness;
        let alpha_squared = alpha * alpha;
        let denominator = ndot_h * ndot_h * (alpha_squared - 1.0) + 1.0;
        let distribution = alpha_squared / (PI * denominator * denominator).max(1.0e-7);

        // Exact Smith G1 for GGX, evaluated for view and light directions.
        let smith_g1 = |ndot_x: f32| {
            let root = (alpha_squared + (1.0 - alpha_squared) * ndot_x * ndot_x)
                .max(0.0)
                .sqrt();
            (2.0 * ndot_x) / (ndot_x + root).max(1.0e-7)
        };
        let geometry = smith_g1(ndot_v) * smith_g1(ndot_l);
        let fresnel = fresnel_schlick(vdot_h, f0);
        let specular = fresnel * (distribution * geometry / (4.0 * ndot_v * ndot_l).max(1.0e-7));
        let diffuse = base_color * (Vec3::ONE - fresnel) * ((1.0 - metallic) / PI);
        direct = (diffuse + specular) * direct_radiance * ndot_l;
    }

    let environment_specular = environment.map_or(Vec3::ZERO, |environment| {
        let reflected = (n * (2.0 * n.dot(v)) - v).normalize_or_zero();
        let fresnel = fresnel_schlick(ndot_v, f0);
        let roughness_fade = 1.0 - 0.5 * roughness;
        fresnel * environment.radiance(reflected, roughness) * roughness_fade
    });

    finite_color(ambient + environment_diffuse + direct + environment_specular + emissive)
}

fn material_surface(material: &Material, uv: Vec2) -> PbrSurface {
    let base_color = material.get(Channel::BaseColor, uv);
    let emissive = material.get(Channel::Emissive, uv);
    PbrSurface {
        base_color: srgb_rgb_to_linear(base_color),
        metallic: material.get(Channel::Metallic, uv)[0] as f32 / 255.0,
        roughness: material.get(Channel::Roughness, uv)[0] as f32 / 255.0,
        ao: material.get(Channel::Occlusion, uv)[0] as f32 / 255.0,
        emissive: srgb_rgb_to_linear(emissive),
    }
}

fn clip_triangle_to_near_plane(vertices: [ClipVertex; 3]) -> Vec<ClipVertex> {
    let mut clipped = Vec::with_capacity(4);
    let mut previous = vertices[2];
    let mut previous_inside = previous.depth >= NEAR_PLANE;
    for current in vertices {
        let current_inside = current.depth >= NEAR_PLANE;
        if current_inside != previous_inside {
            let t = (NEAR_PLANE - previous.depth) / (current.depth - previous.depth);
            clipped.push(ClipVertex {
                p: previous.p.lerp(current.p, t),
                n: previous.n.lerp(current.n, t),
                uv: previous.uv.lerp(current.uv, t),
                tangent: previous.tangent.lerp(current.tangent, t),
                tangent_handedness: previous.tangent_handedness
                    + (current.tangent_handedness - previous.tangent_handedness) * t,
                depth: NEAR_PLANE,
                barycentric: std::array::from_fn(|index| {
                    previous.barycentric[index]
                        + (current.barycentric[index] - previous.barycentric[index]) * t
                }),
            });
        }
        if current_inside {
            clipped.push(current);
        }
        previous = current;
        previous_inside = current_inside;
    }
    clipped
}

pub struct Viewport {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    uv: Vec<Option<[f32; 2]>>,
    material_hits: Vec<Option<usize>>,
    surface_hits: Vec<Option<ScreenSurface>>,
    render_cache_key: Option<RenderCacheKey>,
    visible_transparency: bool,
    lighting: PbrLightingSettings,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}
impl Viewport {
    pub fn new(width: usize, height: usize) -> Self {
        // Match resize bounds at construction time too, so all framebuffer,
        // depth, hit-buffer, and sparse transparency work has a hard ceiling.
        let width = width.clamp(128, MAX_VIEWPORT_WIDTH);
        let height = height.clamp(128, MAX_VIEWPORT_HEIGHT);
        let size = width
            .checked_mul(height)
            .filter(|size| *size <= MAX_VIEWPORT_PIXELS)
            .expect("clamped viewport dimensions must fit the reference rasterizer");
        let rgba_size = size
            .checked_mul(4)
            .expect("clamped viewport byte size must fit usize");
        Self {
            width,
            height,
            rgba: vec![0; rgba_size],
            uv: vec![None; size],
            material_hits: vec![None; size],
            surface_hits: vec![None; size],
            render_cache_key: None,
            visible_transparency: false,
            lighting: PbrLightingSettings::default(),
            yaw: 0.42,
            pitch: 0.25,
            distance: 4.9,
        }
    }
    /// Resize the software framebuffer without resetting the orbit camera.
    /// Keeps hit testing aligned with the displayed 3D editor after docking.
    pub fn resize(&mut self, width: usize, height: usize) {
        let width = width.clamp(128, MAX_VIEWPORT_WIDTH);
        let height = height.clamp(128, MAX_VIEWPORT_HEIGHT);
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        let size = width
            .checked_mul(height)
            .filter(|size| *size <= MAX_VIEWPORT_PIXELS)
            .expect("clamped viewport dimensions must fit the reference rasterizer");
        let rgba_size = size
            .checked_mul(4)
            .expect("clamped viewport byte size must fit usize");
        self.rgba.resize(rgba_size, 0);
        self.uv.resize(size, None);
        self.uv.fill(None);
        self.material_hits.resize(size, None);
        self.material_hits.fill(None);
        self.surface_hits.resize(size, None);
        self.surface_hits.fill(None);
        self.render_cache_key = None;
    }
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw += dx * 0.012;
        self.pitch = (self.pitch - dy * 0.012).clamp(-1.45, 1.45);
        self.render_cache_key = None;
    }
    /// Restore the default inspection angle and distance.
    pub fn reset_camera(&mut self) {
        self.yaw = 0.42;
        self.pitch = 0.25;
        self.distance = 4.9;
        self.render_cache_key = None;
    }
    pub fn lighting_settings(&self) -> PbrLightingSettings {
        self.lighting
    }
    pub fn set_lighting_settings(&mut self, settings: PbrLightingSettings) {
        let settings = settings.validated();
        if self.lighting != settings {
            self.lighting = settings;
            self.render_cache_key = None;
        }
    }
    pub fn zoom(&mut self, delta: f32) {
        self.distance = (self.distance * (-delta * 0.001).exp()).clamp(1.6, 15.0);
        self.render_cache_key = None;
    }
    #[cfg(test)]
    pub fn pick(&self, x: usize, y: usize) -> Option<[f32; 2]> {
        if x >= self.width || y >= self.height {
            None
        } else {
            self.uv[y * self.width + x]
        }
    }
    pub fn pick_surface(&self, x: usize, y: usize) -> Option<PickHit> {
        if x >= self.width || y >= self.height {
            None
        } else {
            self.surface_hits[y * self.width + x].map(|surface| surface.pick)
        }
    }
    #[cfg(test)]
    pub fn pick_material_slot(&self, x: usize, y: usize) -> Option<usize> {
        if x >= self.width || y >= self.height {
            None
        } else {
            self.material_hits[y * self.width + x]
        }
    }

    pub fn render(&mut self, mesh: &Mesh, mat: &Material, solo: Option<Channel>) {
        self.render_cache_key = None;
        self.render_internal(mesh, mat, None, solo, self.lighting.lighting());
    }

    pub fn render_bound(
        &mut self,
        mesh: &Mesh,
        library: &MaterialLibrary,
        bindings: &[Option<u32>],
        solo: Option<Channel>,
    ) {
        self.render_internal(
            mesh,
            library.active(),
            Some((library, bindings)),
            solo,
            self.lighting.lighting(),
        );
        self.render_cache_key = Some(RenderCacheKey {
            active_set_id: library.active_set_id,
            bindings: bindings.to_vec(),
            solo,
            set_dimensions: library.set_dimensions(),
            lighting: self.lighting,
            visible_transparency: self.visible_transparency,
        });
    }

    #[allow(clippy::too_many_arguments)] // Kept aligned with the editor's explicit refresh contract.
    pub fn refresh_material_tiles(
        &mut self,
        library: &MaterialLibrary,
        bindings: &[Option<u32>],
        edited_set_id: u32,
        edited_channel: Channel,
        tile_size: u32,
        tiles: &[(u32, u32, u32, u32)],
        solo: Option<Channel>,
    ) -> ScreenRefresh {
        let Some(key) = self.render_cache_key.as_ref() else {
            return ScreenRefresh::CacheInvalid;
        };
        if key.active_set_id != library.active_set_id
            || key.bindings != bindings
            || key.solo != solo
            || key.lighting != self.lighting
            || key.set_dimensions != library.set_dimensions()
            || edited_channel == Channel::Opacity
            || (key.visible_transparency && solo.is_none())
            || tile_size == 0
            || tiles.is_empty()
        {
            return ScreenRefresh::CacheInvalid;
        }
        let Some(material) = library.sets.get(&edited_set_id) else {
            return ScreenRefresh::CacheInvalid;
        };
        let Some(map) = material.maps.values().next() else {
            return ScreenRefresh::CacheInvalid;
        };
        let width = map.width();
        let height = map.height();
        if width == 0 || height == 0 {
            return ScreenRefresh::CacheInvalid;
        }

        // Validate the complete change list and build its tile-cell lookup
        // before touching the cached frame. Callers normally pass one patch
        // rectangle per tile; normalize each origin exactly as the previous
        // per-hit test did, while bounding temporary memory for malformed or
        // unexpectedly large update lists.
        if tiles.len() > MAX_REFRESH_TILE_CELLS {
            return ScreenRefresh::CacheInvalid;
        }
        for &(x, y, tile_width, tile_height) in tiles {
            if tile_width == 0
                || tile_height == 0
                || x.checked_add(tile_width).is_none_or(|right| right > width)
                || y.checked_add(tile_height)
                    .is_none_or(|bottom| bottom > height)
            {
                return ScreenRefresh::CacheInvalid;
            }
        }
        let mut changed_cells = HashSet::new();
        if changed_cells.try_reserve(tiles.len()).is_err() {
            return ScreenRefresh::CacheInvalid;
        }
        for &(x, y, _, _) in tiles {
            let cell_x = x / tile_size;
            let cell_y = y / tile_size;
            changed_cells.insert((u64::from(cell_y) << 32) | u64::from(cell_x));
        }

        let screen_cell_columns = self.width.div_ceil(SCREEN_DAMAGE_CELL_SIZE);
        let screen_cell_rows = self.height.div_ceil(SCREEN_DAMAGE_CELL_SIZE);
        let Some(screen_cell_count) = screen_cell_columns.checked_mul(screen_cell_rows) else {
            return ScreenRefresh::CacheInvalid;
        };
        let mut damaged_screen_cells = Vec::new();
        if damaged_screen_cells
            .try_reserve_exact(screen_cell_count)
            .is_err()
        {
            return ScreenRefresh::CacheInvalid;
        }
        damaged_screen_cells.resize(screen_cell_count, false);
        let mut has_visible_pixels = false;
        for (index, surface) in self.surface_hits.iter().enumerate() {
            let Some(surface) = surface else { continue };
            let set_id = bindings
                .get(surface.pick.material_slot)
                .copied()
                .flatten()
                .unwrap_or(library.active_set_id);
            if set_id != edited_set_id {
                continue;
            }
            let uv = Vec2::from_array(surface.pick.uv).clamp(Vec2::ZERO, Vec2::ONE);
            let texel_x = (uv.x * (width - 1) as f32).round() as u32;
            let texel_y = (uv.y * (height - 1) as f32).round() as u32;
            let tile_x = texel_x / tile_size;
            let tile_y = texel_y / tile_size;
            let tile = (u64::from(tile_y) << 32) | u64::from(tile_x);
            if !changed_cells.contains(&tile) {
                continue;
            }

            let screen_x = (index % self.width) / SCREEN_DAMAGE_CELL_SIZE;
            let screen_y = (index / self.width) / SCREEN_DAMAGE_CELL_SIZE;
            damaged_screen_cells[screen_y * screen_cell_columns + screen_x] = true;
            has_visible_pixels = true;
        }
        if !has_visible_pixels {
            return ScreenRefresh::NoVisiblePixels;
        }
        let Some(damage_rects) =
            coalesce_screen_damage_cells(&damaged_screen_cells, self.width, self.height)
        else {
            return ScreenRefresh::CacheInvalid;
        };

        let eye = Vec3::new(
            self.distance * self.yaw.sin() * self.pitch.cos(),
            self.distance * self.pitch.sin(),
            self.distance * self.yaw.cos() * self.pitch.cos(),
        );
        let lighting = self.lighting.lighting();
        for (index, surface) in self.surface_hits.iter_mut().enumerate() {
            let Some(surface) = surface else { continue };
            let set_id = bindings
                .get(surface.pick.material_slot)
                .copied()
                .flatten()
                .unwrap_or(library.active_set_id);
            if set_id != edited_set_id {
                continue;
            }
            let uv = Vec2::from_array(surface.pick.uv).clamp(Vec2::ZERO, Vec2::ONE);
            let texel_x = (uv.x * (width - 1) as f32).round() as u32;
            let texel_y = (uv.y * (height - 1) as f32).round() as u32;
            let cell_x = texel_x / tile_size;
            let cell_y = texel_y / tile_size;
            let cell = (u64::from(cell_y) << 32) | u64::from(cell_x);
            if !changed_cells.contains(&cell) {
                continue;
            }
            let rgb = shade_sample(material, surface, eye, solo, lighting);
            let offset = index * 4;
            self.rgba[offset..offset + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        ScreenRefresh::Updated(damage_rects)
    }

    fn render_internal(
        &mut self,
        mesh: &Mesh,
        default_mat: &Material,
        bound: Option<(&MaterialLibrary, &[Option<u32>])>,
        solo: Option<Channel>,
        lighting: PbrLighting,
    ) {
        if !(128..=MAX_VIEWPORT_WIDTH).contains(&self.width)
            || !(128..=MAX_VIEWPORT_HEIGHT).contains(&self.height)
        {
            self.resize(self.width, self.height);
        }
        let size = self.width * self.height;
        if self.rgba.len() != size * 4 {
            self.rgba.resize(size * 4, 0);
        }
        if self.uv.len() != size {
            self.uv.resize(size, None);
        }
        self.uv.fill(None);
        if self.material_hits.len() != size {
            self.material_hits.resize(size, None);
        }
        self.material_hits.fill(None);
        if self.surface_hits.len() != size {
            self.surface_hits.resize(size, None);
        }
        self.surface_hits.fill(None);
        self.visible_transparency = false;
        let mut depths = vec![f32::INFINITY; size];
        let mut transparent_fragments: HashMap<usize, Vec<CompositeFragment>> = HashMap::new();
        for y in 0..self.height {
            for x in 0..self.width {
                let i = (y * self.width + x) * 4;
                let shade = ((y as f32 / self.height as f32) * 14.0).round() as u8;
                self.rgba[i..i + 4].copy_from_slice(&[
                    27 + shade / 3,
                    27 + shade / 3,
                    36 + shade,
                    255,
                ]);
            }
        }
        let eye = Vec3::new(
            self.distance * self.yaw.sin() * self.pitch.cos(),
            self.distance * self.pitch.sin(),
            self.distance * self.yaw.cos() * self.pitch.cos(),
        );
        let forward = (-eye).normalize();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        let f = self.height as f32 * 1.08;
        let project =
            |p: Vec3, n: Vec3, uv: Vec2, tangent: Vec3, tangent_handedness: f32| -> ScreenVertex {
                let rel = p - eye;
                let z = rel.dot(forward);
                ScreenVertex {
                    xy: Vec2::new(
                        self.width as f32 * 0.5 + rel.dot(right) * f / z,
                        self.height as f32 * 0.5 - rel.dot(up) * f / z,
                    ),
                    z,
                    uv,
                    n,
                    tangent,
                    tangent_handedness,
                    p,
                    barycentric: [0.0; 3],
                }
            };
        for (source_triangle_index, face) in mesh.indices.chunks_exact(3).enumerate() {
            let material_slot = mesh.triangle_material(source_triangle_index);
            let mat = bound
                .map(|(library, bindings)| library.resolve(material_slot, bindings))
                .unwrap_or(default_mat);
            let clipped = clip_triangle_to_near_plane(std::array::from_fn(|corner| {
                let index = face[corner];
                let vertex = &mesh.vertices[index as usize];
                let p = Vec3::from_array(vertex.position);
                ClipVertex {
                    p,
                    n: Vec3::from_array(vertex.normal),
                    uv: Vec2::from_array(vertex.uv),
                    tangent: vertex
                        .tangent
                        .map(|tangent| Vec3::new(tangent[0], tangent[1], tangent[2]))
                        .unwrap_or(Vec3::ZERO),
                    tangent_handedness: vertex.tangent.map_or(1.0, |tangent| tangent[3]),
                    depth: (p - eye).dot(forward),
                    barycentric: std::array::from_fn(
                        |index| {
                            if index == corner {
                                1.0
                            } else {
                                0.0
                            }
                        },
                    ),
                }
            }));
            if clipped.len() < 3 {
                continue;
            }
            for fan_index in 1..clipped.len() - 1 {
                let mut a = project(
                    clipped[0].p,
                    clipped[0].n,
                    clipped[0].uv,
                    clipped[0].tangent,
                    clipped[0].tangent_handedness,
                );
                let mut b = project(
                    clipped[fan_index].p,
                    clipped[fan_index].n,
                    clipped[fan_index].uv,
                    clipped[fan_index].tangent,
                    clipped[fan_index].tangent_handedness,
                );
                let mut c = project(
                    clipped[fan_index + 1].p,
                    clipped[fan_index + 1].n,
                    clipped[fan_index + 1].uv,
                    clipped[fan_index + 1].tangent,
                    clipped[fan_index + 1].tangent_handedness,
                );
                a.barycentric = clipped[0].barycentric;
                b.barycentric = clipped[fan_index].barycentric;
                c.barycentric = clipped[fan_index + 1].barycentric;
                if !a.xy.is_finite() || !b.xy.is_finite() || !c.xy.is_finite() {
                    continue;
                }
                let edge = |p: Vec2, q: Vec2, r: Vec2| {
                    (r.x - p.x) * (q.y - p.y) - (r.y - p.y) * (q.x - p.x)
                };
                let area = edge(a.xy, b.xy, c.xy);
                if area.abs() < 0.0001 {
                    continue;
                }
                let x0 = a.xy.x.min(b.xy.x).min(c.xy.x).floor().max(0.0) as usize;
                let x1 =
                    a.xy.x
                        .max(b.xy.x)
                        .max(c.xy.x)
                        .ceil()
                        .min((self.width - 1) as f32) as usize;
                let y0 = a.xy.y.min(b.xy.y).min(c.xy.y).floor().max(0.0) as usize;
                let y1 =
                    a.xy.y
                        .max(b.xy.y)
                        .max(c.xy.y)
                        .ceil()
                        .min((self.height - 1) as f32) as usize;
                if x0 > x1 || y0 > y1 {
                    continue;
                }
                // Approximate per-triangle tangent basis for tangent-space normals.
                let dp1 = b.p - a.p;
                let dp2 = c.p - a.p;
                let dt1 = b.uv - a.uv;
                let dt2 = c.uv - a.uv;
                let det = dt1.x * dt2.y - dt1.y * dt2.x;
                let (fallback_tangent, fallback_tangent_handedness) = if det.abs() > 0.000001 {
                    let tangent = (dp1 * dt2.y - dp2 * dt1.y) / det;
                    let bitangent = (dp2 * dt1.x - dp1 * dt2.x) / det;
                    let face_normal = (a.n + b.n + c.n).normalize_or_zero();
                    let handedness = if face_normal.cross(tangent).dot(bitangent) < 0.0 {
                        -1.0
                    } else {
                        1.0
                    };
                    (tangent.normalize_or_zero(), handedness)
                } else {
                    (Vec3::X, 1.0)
                };
                let has_vertex_tangents = face
                    .iter()
                    .all(|&index| mesh.vertices[index as usize].tangent.is_some());
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        let sample = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
                        let u = edge(b.xy, c.xy, sample) / area;
                        let v = edge(c.xy, a.xy, sample) / area;
                        let w = 1.0 - u - v;
                        if u < -0.00001 || v < -0.00001 || w < -0.00001 {
                            continue;
                        }
                        let ia = u / a.z;
                        let ib = v / b.z;
                        let ic = w / c.z;
                        let inv_z = ia + ib + ic;
                        if inv_z < 0.000001 {
                            continue;
                        }
                        let z = 1.0 / inv_z;
                        let idx = y * self.width + x;
                        let nearest = z < depths[idx];
                        let previous_nearest = if nearest && depths[idx].is_finite() {
                            self.surface_hits[idx].map(|_| {
                                let offset = idx * 4;
                                CompositeFragment {
                                    depth: depths[idx],
                                    color: [
                                        self.rgba[offset],
                                        self.rgba[offset + 1],
                                        self.rgba[offset + 2],
                                    ],
                                    opacity: 255,
                                }
                            })
                        } else {
                            None
                        };
                        if !nearest && !transparent_fragments.contains_key(&idx) {
                            continue;
                        }
                        let uv = (a.uv * ia + b.uv * ib + c.uv * ic) * z;
                        let barycentric = std::array::from_fn(|component| {
                            (a.barycentric[component] * ia
                                + b.barycentric[component] * ib
                                + c.barycentric[component] * ic)
                                * z
                        });
                        let n = (a.n * ia + b.n * ib + c.n * ic).normalize_or_zero();
                        let (tangent, tangent_handedness) = if has_vertex_tangents {
                            (
                                (a.tangent * ia + b.tangent * ib + c.tangent * ic) * z,
                                (a.tangent_handedness * ia
                                    + b.tangent_handedness * ib
                                    + c.tangent_handedness * ic)
                                    * z,
                            )
                        } else {
                            (fallback_tangent, fallback_tangent_handedness)
                        };
                        let p = (a.p * ia + b.p * ib + c.p * ic) * z;
                        let surface = ScreenSurface {
                            pick: PickHit {
                                uv: uv.to_array(),
                                material_slot,
                                triangle_index: source_triangle_index,
                                barycentric,
                            },
                            position: p,
                            normal: n,
                            tangent,
                            tangent_handedness,
                        };
                        let rgb = shade_sample(mat, &surface, eye, solo, lighting);
                        let opacity = if solo.is_none() {
                            mat.get(Channel::Opacity, uv)[0]
                        } else {
                            255
                        };

                        if nearest {
                            depths[idx] = z;
                            self.uv[idx] = Some(uv.to_array());
                            self.material_hits[idx] = Some(material_slot);
                            self.surface_hits[idx] = Some(surface);
                            let off = idx * 4;
                            self.rgba[off..off + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                        }

                        if solo.is_none() && opacity < 255 {
                            let fragments = transparent_fragments.entry(idx).or_default();
                            if fragments.is_empty() {
                                if let Some(previous) = previous_nearest {
                                    insert_composite_fragment(fragments, previous);
                                }
                            }
                            insert_composite_fragment(
                                fragments,
                                CompositeFragment {
                                    depth: z,
                                    color: rgb,
                                    opacity,
                                },
                            );
                        } else if let Some(fragments) = transparent_fragments.get_mut(&idx) {
                            insert_composite_fragment(
                                fragments,
                                CompositeFragment {
                                    depth: z,
                                    color: rgb,
                                    opacity,
                                },
                            );
                        }
                    }
                }
            }
        }
        for (idx, fragments) in transparent_fragments {
            let (rgba, visible_transparency) =
                composite_fragments(&fragments, self.width, self.height, idx);
            self.visible_transparency |= visible_transparency;
            self.rgba[idx * 4..idx * 4 + 4].copy_from_slice(&rgba);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analytic_lighting() -> PbrLighting {
        PbrLighting {
            direction_to_light: Vec3::Z,
            direct_radiance: Vec3::ONE,
            ambient_irradiance: Vec3::ZERO,
            environment: None,
        }
    }

    fn analytic_surface(base_color: Vec3, metallic: f32, roughness: f32) -> PbrSurface {
        PbrSurface {
            base_color,
            metallic,
            roughness,
            ao: 1.0,
            emissive: Vec3::ZERO,
        }
    }

    fn assert_vec3_near(actual: Vec3, expected: Vec3, epsilon: f32) {
        assert!(
            (actual - expected).abs().max_element() <= epsilon,
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn ggx_dielectric_normal_incidence_matches_analytic_oracle() {
        let shaded = shade_pbr(
            analytic_surface(Vec3::splat(0.5), 0.0, 1.0),
            Vec3::Z,
            Vec3::Z,
            analytic_lighting(),
        );
        let expected = 0.96 * 0.5 / PI + 0.04 / (4.0 * PI);
        assert_vec3_near(shaded, Vec3::splat(expected), 1.0e-6);
    }

    #[test]
    fn ggx_metallic_normal_incidence_has_no_diffuse_term() {
        let base = Vec3::new(0.8, 0.4, 0.2);
        let shaded = shade_pbr(
            analytic_surface(base, 1.0, 1.0),
            Vec3::Z,
            Vec3::Z,
            analytic_lighting(),
        );
        assert_vec3_near(shaded, base / (4.0 * PI), 1.0e-6);
    }

    #[test]
    fn ggx_roughness_changes_normal_incidence_lobe_by_expected_amount() {
        let shade = |roughness| {
            shade_pbr(
                analytic_surface(Vec3::splat(0.5), 0.0, roughness),
                Vec3::Z,
                Vec3::Z,
                analytic_lighting(),
            )
        };
        let smooth = shade(0.5);
        let rough = shade(1.0);
        assert_vec3_near(
            smooth,
            Vec3::splat(0.96 * 0.5 / PI + 0.04 * (1.0 / (PI * 0.0625)) / 4.0),
            1.0e-6,
        );
        assert_vec3_near(
            rough,
            Vec3::splat(0.96 * 0.5 / PI + 0.04 / (4.0 * PI)),
            1.0e-6,
        );
        assert!(smooth.x > rough.x);
    }

    #[test]
    fn ggx_grazing_configuration_matches_numeric_oracle_and_stays_finite() {
        let grazing = Vec3::new(3.0_f32.sqrt() * 0.5, 0.0, 0.5);
        let shaded = shade_pbr(
            analytic_surface(Vec3::splat(0.5), 0.0, 0.5),
            Vec3::Z,
            grazing,
            PbrLighting {
                direction_to_light: grazing,
                ..analytic_lighting()
            },
        );
        assert_vec3_near(shaded, Vec3::splat(0.07701611), 2.0e-6);
        assert!(shaded.is_finite());

        let below_surface = shade_pbr(
            analytic_surface(Vec3::splat(0.5), 0.0, 0.5),
            Vec3::Z,
            Vec3::Z,
            PbrLighting {
                direction_to_light: -Vec3::Z,
                ..analytic_lighting()
            },
        );
        assert_eq!(below_surface, Vec3::ZERO);
    }

    #[test]
    fn ambient_occlusion_only_changes_ambient_fill() {
        let mut surface = analytic_surface(Vec3::splat(0.5), 0.0, 1.0);
        let lighting = PbrLighting {
            direct_radiance: Vec3::ZERO,
            ambient_irradiance: Vec3::ONE,
            ..analytic_lighting()
        };
        surface.ao = 0.0;
        let occluded = shade_pbr(surface, Vec3::Z, Vec3::Z, lighting);
        surface.ao = 1.0;
        let unoccluded = shade_pbr(surface, Vec3::Z, Vec3::Z, lighting);
        assert_eq!(occluded, Vec3::ZERO);
        assert_vec3_near(unoccluded, Vec3::splat(0.96 * 0.5 / PI), 1.0e-6);

        let direct_only = analytic_lighting();
        surface.ao = 0.0;
        let with_ao_zero = shade_pbr(surface, Vec3::Z, Vec3::Z, direct_only);
        surface.ao = 1.0;
        let with_ao_one = shade_pbr(surface, Vec3::Z, Vec3::Z, direct_only);
        assert_eq!(with_ao_zero, with_ao_one);
    }

    #[test]
    fn invalid_surface_values_are_safely_bounded() {
        let result = shade_pbr(
            PbrSurface {
                base_color: Vec3::splat(f32::NAN),
                metallic: f32::NAN,
                roughness: f32::NAN,
                ao: f32::INFINITY,
                emissive: Vec3::splat(f32::INFINITY),
            },
            Vec3::splat(f32::NAN),
            Vec3::Z,
            analytic_lighting(),
        );
        assert!(result.is_finite());
        assert_eq!(result, Vec3::ZERO);

        let minimum_roughness = shade_pbr(
            analytic_surface(Vec3::splat(0.5), 0.0, 0.0),
            Vec3::Z,
            Vec3::Z,
            analytic_lighting(),
        );
        assert!(minimum_roughness.is_finite());
    }

    #[test]
    fn srgba_transfer_functions_round_trip_middle_gray() {
        let linear = srgb_channel_to_linear(128);
        assert!((linear - 0.2158605).abs() < 1.0e-6);
        assert_eq!(linear_channel_to_srgb(linear), 128);
    }

    fn screen_position(view: &Viewport, x: f32, y: f32, depth: f32) -> [f32; 3] {
        let eye = Vec3::new(
            view.distance * view.yaw.sin() * view.pitch.cos(),
            view.distance * view.pitch.sin(),
            view.distance * view.yaw.cos() * view.pitch.cos(),
        );
        let forward = (-eye).normalize();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        let focal_length = view.height as f32 * 1.08;
        let screen_right = (x - view.width as f32 * 0.5) * depth / focal_length;
        let screen_up = (view.height as f32 * 0.5 - y) * depth / focal_length;
        (eye + forward * depth + right * screen_right + up * screen_up).to_array()
    }

    fn triangle_mesh(view: &Viewport, depth: f32, uvs: [[f32; 2]; 3], material_slot: u32) -> Mesh {
        triangle_mesh_with_depths(view, [depth; 3], uvs, material_slot)
    }

    fn triangle_mesh_with_depths(
        view: &Viewport,
        depths: [f32; 3],
        uvs: [[f32; 2]; 3],
        material_slot: u32,
    ) -> Mesh {
        let screen = [[20.5, 20.5], [180.5, 20.5], [100.5, 180.5]];
        Mesh {
            name: "analytic picking triangle".into(),
            vertices: screen
                .into_iter()
                .zip(uvs)
                .zip(depths)
                .map(|((point, uv), depth)| crate::mesh::Vertex {
                    position: screen_position(view, point[0], point[1], depth),
                    normal: [0.0, 0.0, 1.0],
                    uv,
                    tangent: None,
                    extensions: Default::default(),
                })
                .collect(),
            indices: vec![0, 1, 2],
            materials: vec!["first".into(), "picked".into()],
            triangle_materials: vec![material_slot],
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        }
    }

    fn overlapping_triangle_mesh(view: &Viewport) -> Mesh {
        let mut front = triangle_mesh(view, 3.0, [[0.5; 2]; 3], 0);
        let rear = triangle_mesh(view, 3.6, [[0.5; 2]; 3], 1);
        let base = front.vertices.len() as u32;
        front.vertices.extend(rear.vertices);
        front
            .indices
            .extend(rear.indices.into_iter().map(|index| index + base));
        front.materials = vec!["front".into(), "rear".into()];
        front.triangle_materials = vec![0, 1];
        front
    }

    fn solid_material(color: [u8; 4], opacity: [u8; 4]) -> Material {
        let maps = Channel::ALL
            .into_iter()
            .map(|channel| {
                let value = match channel {
                    Channel::BaseColor => color,
                    Channel::Opacity => opacity,
                    Channel::Normal => [128, 128, 255, 255],
                    Channel::Roughness => [255; 4],
                    Channel::Occlusion => [255; 4],
                    _ => [0, 0, 0, 255],
                };
                (channel, RgbaImage::from_pixel(2, 2, image::Rgba(value)))
            })
            .collect();
        Material { maps }
    }

    fn overlapping_library(front_opacity: u8) -> MaterialLibrary {
        MaterialLibrary {
            active_set_id: 0,
            sets: HashMap::from([
                (
                    0,
                    solid_material([230, 30, 20, 255], [front_opacity, 25, 75, 255]),
                ),
                (1, solid_material([20, 40, 230, 255], [255; 4])),
            ]),
        }
    }

    fn render_overlap(front_opacity: u8) -> (Viewport, Mesh, MaterialLibrary) {
        let mut viewport = Viewport::new(200, 200);
        let mesh = overlapping_triangle_mesh(&viewport);
        let library = overlapping_library(front_opacity);
        viewport.render_bound(&mesh, &library, &[Some(0), Some(1)], None);
        (viewport, mesh, library)
    }

    #[test]
    fn opacity_zero_reveals_rear_surface_and_keeps_nearest_pick() {
        let (transparent, _, _) = render_overlap(0);
        let (rear, _, rear_library) = render_overlap(255);
        // Render the rear-only comparison using its actual slot material.
        let rear_only_mesh = triangle_mesh(&Viewport::new(200, 200), 3.6, [[0.5; 2]; 3], 0);
        let mut rear_only = Viewport::new(200, 200);
        rear_only.render_bound(&rear_only_mesh, &rear_library, &[Some(1)], None);
        let pixel = (100 * transparent.width + 100) * 4;
        assert_eq!(
            &transparent.rgba[pixel..pixel + 3],
            &rear_only.rgba[pixel..pixel + 3]
        );
        assert_eq!(
            transparent.pick_surface(100, 100).unwrap().triangle_index,
            0
        );
        assert!(transparent.visible_transparency);
        assert!(!rear.visible_transparency);
    }

    #[test]
    fn opacity_half_composites_front_over_rear_and_full_opacity_keeps_opaque_bytes() {
        let (half, _, _) = render_overlap(128);
        let (opaque, _, opaque_library) = render_overlap(255);
        let front_mesh = triangle_mesh(&Viewport::new(200, 200), 3.0, [[0.5; 2]; 3], 0);
        let rear_mesh = triangle_mesh(&Viewport::new(200, 200), 3.6, [[0.5; 2]; 3], 0);
        let mut front_only = Viewport::new(200, 200);
        front_only.render_bound(&front_mesh, &opaque_library, &[Some(0)], None);
        let mut rear_only = Viewport::new(200, 200);
        rear_only.render_bound(&rear_mesh, &opaque_library, &[Some(1)], None);
        let pixel = (100 * half.width + 100) * 4;
        let expected = linear_rgb_to_srgb(
            srgb_rgb_to_linear([
                front_only.rgba[pixel],
                front_only.rgba[pixel + 1],
                front_only.rgba[pixel + 2],
                255,
            ]) * (128.0 / 255.0)
                + srgb_rgb_to_linear([
                    rear_only.rgba[pixel],
                    rear_only.rgba[pixel + 1],
                    rear_only.rgba[pixel + 2],
                    255,
                ]) * (127.0 / 255.0),
        );
        assert_eq!(&half.rgba[pixel..pixel + 3], &expected);
        assert!(half.visible_transparency);
        let (repeated_half, _, _) = render_overlap(128);
        assert_eq!(half.rgba, repeated_half.rgba);
        assert_eq!(opaque.pick_surface(100, 100).unwrap().triangle_index, 0);
        assert_eq!(opaque.rgba, front_only.rgba);
        assert!(!opaque.visible_transparency);
    }

    #[test]
    fn transparent_compositor_retains_only_eight_nearest_layers_in_depth_order() {
        let mut fragments = Vec::new();
        for depth in (1..=12).rev() {
            insert_composite_fragment(
                &mut fragments,
                CompositeFragment {
                    depth: depth as f32,
                    color: [depth as u8; 3],
                    opacity: 128,
                },
            );
        }
        assert_eq!(fragments.len(), MAX_COMPOSITING_LAYERS_PER_PIXEL);
        assert_eq!(
            fragments
                .iter()
                .map(|fragment| fragment.depth)
                .collect::<Vec<_>>(),
            (1..=8).map(|depth| depth as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn solo_opacity_displays_the_raw_channel_without_transparency_compositing() {
        let viewport = Viewport::new(200, 200);
        let mesh = overlapping_triangle_mesh(&viewport);
        let library = overlapping_library(0);
        let mut solo = Viewport::new(200, 200);
        solo.render_bound(&mesh, &library, &[Some(0), Some(1)], Some(Channel::Opacity));
        let pixel = (100 * solo.width + 100) * 4;
        assert_eq!(&solo.rgba[pixel..pixel + 3], &[0, 25, 75]);
        assert!(!solo.visible_transparency);
        assert_eq!(solo.pick_surface(100, 100).unwrap().triangle_index, 0);
    }

    #[test]
    fn opacity_edits_and_opaque_channel_edits_in_transparent_view_invalidate_cache() {
        let (mut view, _mesh, mut library) = render_overlap(255);
        let original = view.rgba.clone();
        let replacement = RgbaImage::from_pixel(2, 2, image::Rgba([0, 255, 0, 255]));
        assert!(library.patch_region(0, Channel::Opacity, 0, 0, &replacement));
        assert_eq!(
            view.refresh_material_tiles(
                &library,
                &[Some(0), Some(1)],
                0,
                Channel::Opacity,
                2,
                &[(0, 0, 2, 2)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
        assert_eq!(view.rgba, original);

        let (mut transparent_view, mesh, mut transparent_library) = render_overlap(128);
        let cached_frame = transparent_view.rgba.clone();
        let replacement = RgbaImage::from_pixel(2, 2, image::Rgba([30, 240, 70, 255]));
        assert!(transparent_library.patch_region(0, Channel::BaseColor, 0, 0, &replacement));
        assert_eq!(
            transparent_view.refresh_material_tiles(
                &transparent_library,
                &[Some(0), Some(1)],
                0,
                Channel::BaseColor,
                2,
                &[(0, 0, 2, 2)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
        assert_eq!(transparent_view.rgba, cached_frame);
        let mut full = Viewport::new(200, 200);
        full.render_bound(&mesh, &transparent_library, &[Some(0), Some(1)], None);
        assert_ne!(transparent_view.rgba, full.rgba);
        transparent_view.render_bound(&mesh, &transparent_library, &[Some(0), Some(1)], None);
        assert_eq!(transparent_view.rgba, full.rgba);
    }

    #[test]
    fn opaque_channel_tile_refresh_still_matches_full_render() {
        let (mut view, mesh, mut library) = render_overlap(255);
        let replacement = RgbaImage::from_pixel(2, 2, image::Rgba([30, 240, 70, 255]));
        assert!(library.patch_region(0, Channel::BaseColor, 0, 0, &replacement));
        assert!(matches!(
            view.refresh_material_tiles(
                &library,
                &[Some(0), Some(1)],
                0,
                Channel::BaseColor,
                2,
                &[(0, 0, 2, 2)],
                None,
            ),
            ScreenRefresh::Updated(_)
        ));
        let mut full = Viewport::new(200, 200);
        full.render_bound(&mesh, &library, &[Some(0), Some(1)], None);
        assert_eq!(view.rgba, full.rgba);
    }

    fn ray_intersection_barycentric(
        view: &Viewport,
        mesh: &Mesh,
        x: usize,
        y: usize,
    ) -> Option<[f32; 3]> {
        let eye = Vec3::new(
            view.distance * view.yaw.sin() * view.pitch.cos(),
            view.distance * view.pitch.sin(),
            view.distance * view.yaw.cos() * view.pitch.cos(),
        );
        let forward = (-eye).normalize();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        let focal_length = view.height as f32 * 1.08;
        let direction = (forward
            + right * ((x as f32 + 0.5 - view.width as f32 * 0.5) / focal_length)
            + up * ((view.height as f32 * 0.5 - y as f32 - 0.5) / focal_length))
            .normalize();

        let face = &mesh.indices[..3];
        let a = &mesh.vertices[face[0] as usize];
        let b = &mesh.vertices[face[1] as usize];
        let c = &mesh.vertices[face[2] as usize];
        let p0 = Vec3::from_array(a.position);
        let edge1 = Vec3::from_array(b.position) - p0;
        let edge2 = Vec3::from_array(c.position) - p0;
        let h = direction.cross(edge2);
        let determinant = edge1.dot(h);
        if determinant.abs() < 1e-7 {
            return None;
        }
        let inverse = determinant.recip();
        let from_a = eye - p0;
        let beta = from_a.dot(h) * inverse;
        let q = from_a.cross(edge1);
        let gamma = direction.dot(q) * inverse;
        let distance = edge2.dot(q) * inverse;
        const BARYCENTRIC_EPSILON: f32 = 0.00001;
        if beta < -BARYCENTRIC_EPSILON
            || gamma < -BARYCENTRIC_EPSILON
            || beta + gamma > 1.0 + BARYCENTRIC_EPSILON
            || distance <= 0.0
        {
            return None;
        }
        Some([1.0 - beta - gamma, beta, gamma])
    }

    fn ray_intersection_uv(view: &Viewport, mesh: &Mesh, x: usize, y: usize) -> Option<[f32; 2]> {
        let [alpha, beta, gamma] = ray_intersection_barycentric(view, mesh, x, y)?;
        let face = &mesh.indices[..3];
        let a = &mesh.vertices[face[0] as usize];
        let b = &mesh.vertices[face[1] as usize];
        let c = &mesh.vertices[face[2] as usize];
        let uv = Vec2::from_array(a.uv) * alpha
            + Vec2::from_array(b.uv) * beta
            + Vec2::from_array(c.uv) * gamma;
        Some(uv.to_array())
    }

    #[test]
    fn resizing_preserves_camera_and_pick_buffer_dimensions() {
        let mut view = Viewport::new(200, 200);
        view.orbit(10.0, -5.0);
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        view.render(&doc.mesh, &material, None);
        assert!(view.surface_hits.iter().any(Option::is_some));
        let yaw = view.yaw;
        let pitch = view.pitch;
        view.resize(480, 320);
        assert_eq!((view.width, view.height), (480, 320));
        assert_eq!(view.yaw, yaw);
        assert_eq!(view.pitch, pitch);
        assert!(view.pick(481, 200).is_none());
        assert_eq!(view.rgba.len(), 480 * 320 * 4);
        assert_eq!(view.material_hits.len(), 480 * 320);
        assert_eq!(view.surface_hits.len(), 480 * 320);
        assert!(view.surface_hits.iter().all(Option::is_none));
    }
    #[test]
    fn renderer_uses_bound_set_even_when_it_is_not_active() {
        let mut doc = Document::new(64);
        doc.mesh.materials.push("Leather".into());
        doc.mesh.triangle_materials[0] = 1;
        doc.material_bindings.push(None);
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(32, 32, image::Rgba([10, 20, 30, 255]));
        let leather = doc.add_texture_set("Leather").unwrap();
        doc.selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(32, 32, image::Rgba([150, 50, 30, 255]));
        doc.bind_material(1, Some(leather)).unwrap();
        doc.activate_texture_set(1).unwrap();
        let lib = MaterialLibrary::from_document(&doc);
        assert_eq!(
            lib.resolve(0, &doc.material_bindings)
                .get(Channel::BaseColor, Vec2::splat(0.5)),
            [10, 20, 30, 255]
        );
        assert_eq!(
            lib.resolve(1, &doc.material_bindings)
                .get(Channel::BaseColor, Vec2::splat(0.5)),
            [150, 50, 30, 255]
        );
    }
    #[test]
    fn multi_material_renderer_preserves_material_pick_slots() {
        let mut doc = Document::new(64);
        doc.mesh.materials.push("Second".into());
        doc.mesh.triangle_materials[0] = 1;
        doc.material_bindings.push(None);
        let second = doc.add_texture_set("Second").unwrap();
        doc.bind_material(1, Some(second)).unwrap();
        let lib = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(300, 300);
        view.render_bound(&doc.mesh, &lib, &doc.material_bindings, None);
        assert!(view.material_hits.contains(&Some(0)));
        assert!(view.material_hits.contains(&Some(1)));
    }
    #[test]
    fn rendered_cube_has_hit_pixels() {
        let doc = Document::new(64);
        let mat = Material::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        view.render(&doc.mesh, &mat, None);
        assert!(view.uv.iter().any(|uv| uv.is_some()));
    }

    #[test]
    fn viewport_constructor_clamps_dimensions_before_allocating_buffers() {
        let viewport = Viewport::new(usize::MAX, 1);
        assert_eq!(viewport.width, MAX_VIEWPORT_WIDTH);
        assert_eq!(viewport.height, 128);
        assert!(viewport.rgba.len() <= MAX_VIEWPORT_PIXELS * 4);
        assert!(viewport.surface_hits.len() <= MAX_VIEWPORT_PIXELS);
    }

    #[test]
    fn reference_pbr_render_is_deterministic_and_solo_maps_stay_unmodified() {
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut first = Viewport::new(200, 200);
        first.render(&doc.mesh, &material, None);
        let center = (100 * first.width + 100) * 4;
        let reference_rgb = [169_u8, 167, 187];
        for (actual, expected) in first.rgba[center..center + 3].iter().zip(reference_rgb) {
            assert!(
                actual.abs_diff(expected) <= 1,
                "PBR reference pixel drifted: actual {:?}",
                &first.rgba[center..center + 3]
            );
        }
        let mut second = Viewport::new(200, 200);
        second.render(&doc.mesh, &material, None);
        assert_eq!(first.rgba, second.rgba);

        let hit = first
            .uv
            .iter()
            .position(Option::is_some)
            .expect("reference fixture should cover pixels");
        let (x, y) = (hit % first.width, hit / first.width);
        let uv = first.pick(x, y).expect("hit buffer records the same pixel");
        let raw_base_color = material.get(Channel::BaseColor, Vec2::from_array(uv));
        let pbr_rgb = &first.rgba[hit * 4..hit * 4 + 3];
        let mut solo = Viewport::new(200, 200);
        solo.render(&doc.mesh, &material, Some(Channel::BaseColor));
        assert_eq!(&solo.rgba[hit * 4..hit * 4 + 3], &raw_base_color[..3]);
        assert_ne!(pbr_rgb, &raw_base_color[..3]);
    }

    fn uniform_pbr_reference_frame(
        base_color: [u8; 3],
        metallic: u8,
        roughness: u8,
        emissive: [u8; 3],
    ) -> Viewport {
        let mut doc = Document::new(64);
        let layer = doc.selected_layer_mut().unwrap();
        for (channel, color) in [
            (
                Channel::BaseColor,
                [base_color[0], base_color[1], base_color[2], 255],
            ),
            (Channel::Metallic, [metallic, metallic, metallic, 255]),
            (Channel::Roughness, [roughness, roughness, roughness, 255]),
            (
                Channel::Emissive,
                [emissive[0], emissive[1], emissive[2], 255],
            ),
        ] {
            for pixel in layer.pixels_mut(channel, 64).pixels_mut() {
                *pixel = image::Rgba(color);
            }
        }
        let material = Material::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        view.render(&doc.mesh, &material, None);
        view
    }

    fn surface_region_pixel_delta(
        a: &Viewport,
        b: &Viewport,
        rect: (usize, usize, usize, usize),
    ) -> f32 {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let (left, top, right, bottom) = rect;
        let mut delta = 0_u64;
        let mut samples = 0_u64;
        for y in top..bottom {
            for x in left..right {
                let index = y * a.width + x;
                if a.surface_hits[index].is_none() || b.surface_hits[index].is_none() {
                    continue;
                }
                let offset = index * 4;
                for channel in 0..3 {
                    delta += u64::from(a.rgba[offset + channel].abs_diff(b.rgba[offset + channel]));
                    samples += 1;
                }
            }
        }
        assert!(
            samples >= 300,
            "reference region {rect:?} must include at least 100 surface pixels"
        );
        delta as f32 / samples as f32
    }

    #[test]
    fn controlled_full_frame_pbr_material_fixtures_are_repeatable_and_parameter_sensitive() {
        // These fixed screen regions cover the front and two angled cube faces.
        // Comparisons use the surface mask so the viewport background cannot
        // hide a material regression by diluting the measured response.
        const REGIONS: [(usize, usize, usize, usize); 3] =
            [(65, 65, 135, 135), (45, 70, 90, 130), (110, 70, 155, 130)];
        const BASE_COLOR: [u8; 3] = [170, 82, 38];

        let dielectric = uniform_pbr_reference_frame(BASE_COLOR, 0, 128, [0, 0, 0]);
        let dielectric_repeat = uniform_pbr_reference_frame(BASE_COLOR, 0, 128, [0, 0, 0]);
        let metal = uniform_pbr_reference_frame(BASE_COLOR, 255, 128, [0, 0, 0]);
        let smooth = uniform_pbr_reference_frame(BASE_COLOR, 0, 24, [0, 0, 0]);
        let rough = uniform_pbr_reference_frame(BASE_COLOR, 0, 235, [0, 0, 0]);
        let emissive = uniform_pbr_reference_frame(BASE_COLOR, 0, 128, [48, 18, 7]);

        for view in [
            &dielectric,
            &dielectric_repeat,
            &metal,
            &smooth,
            &rough,
            &emissive,
        ] {
            assert_eq!((view.width, view.height), (200, 200));
            assert_eq!(view.rgba.len(), 200 * 200 * 4);
            assert!(view.surface_hits.iter().filter(|hit| hit.is_some()).count() > 2_000);
        }
        assert_eq!(dielectric.rgba, dielectric_repeat.rgba);

        for region in REGIONS {
            assert!(
                surface_region_pixel_delta(&dielectric, &metal, region) > 0.25,
                "metallic response disappeared in region {region:?}"
            );
            assert!(
                surface_region_pixel_delta(&smooth, &rough, region) > 0.25,
                "roughness response disappeared in region {region:?}"
            );
            assert!(
                surface_region_pixel_delta(&dielectric, &emissive, region) > 0.25,
                "emissive response disappeared in region {region:?}"
            );
        }
    }

    #[test]
    fn configurable_reference_lighting_changes_pbr_and_invalidates_incremental_cache() {
        let doc = Document::new(64);
        let library = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        view.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        let default_frame = view.rgba.clone();
        let hit = view
            .surface_hits
            .iter()
            .position(Option::is_some)
            .expect("reference mesh covers viewport pixels");
        let pixel = (
            view.rgba[hit * 4],
            view.rgba[hit * 4 + 1],
            view.rgba[hit * 4 + 2],
        );

        let mut lighting = view.lighting_settings();
        lighting.key_azimuth_degrees = -125.0;
        lighting.key_elevation_degrees = 18.0;
        lighting.key_intensity = 2.7;
        lighting.ambient_fill = 0.12;
        view.set_lighting_settings(lighting);
        assert_eq!(
            view.refresh_material_tiles(
                &library,
                &doc.material_bindings,
                doc.active_set_id,
                Channel::BaseColor,
                16,
                &[(0, 0, 16, 16)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
        view.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        assert_ne!(view.rgba, default_frame);
        assert_ne!(
            pixel,
            (
                view.rgba[hit * 4],
                view.rgba[hit * 4 + 1],
                view.rgba[hit * 4 + 2]
            )
        );

        let mut solo_before = Viewport::new(200, 200);
        solo_before.render_bound(
            &doc.mesh,
            &library,
            &doc.material_bindings,
            Some(Channel::BaseColor),
        );
        let mut solo_after = Viewport::new(200, 200);
        solo_after.set_lighting_settings(lighting);
        solo_after.render_bound(
            &doc.mesh,
            &library,
            &doc.material_bindings,
            Some(Channel::BaseColor),
        );
        assert_eq!(solo_before.rgba, solo_after.rgba);
    }

    #[test]
    fn lighting_settings_are_bounded_and_non_finite_values_use_defaults() {
        let mut view = Viewport::new(128, 128);
        view.set_lighting_settings(PbrLightingSettings {
            key_azimuth_degrees: f32::NAN,
            key_elevation_degrees: 500.0,
            key_intensity: f32::INFINITY,
            ambient_fill: -10.0,
            environment: EnvironmentPreset::Cool,
        });
        let actual = view.lighting_settings();
        let defaults = PbrLightingSettings::default();
        assert_eq!(actual.key_azimuth_degrees, defaults.key_azimuth_degrees);
        assert_eq!(actual.key_elevation_degrees, 89.0);
        assert_eq!(actual.key_intensity, defaults.key_intensity);
        assert_eq!(actual.ambient_fill, 0.0);
        assert_eq!(actual.environment, EnvironmentPreset::Cool);
    }

    #[test]
    fn analytic_environment_presets_change_pixels_and_repeat_deterministically() {
        let doc = Document::new(64);
        let library = MaterialLibrary::from_document(&doc);
        let mut studio = Viewport::new(200, 200);
        studio.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        let studio_frame = studio.rgba.clone();

        let mut warm = Viewport::new(200, 200);
        warm.set_lighting_settings(PbrLightingSettings {
            environment: EnvironmentPreset::Warm,
            ..PbrLightingSettings::default()
        });
        warm.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        assert_ne!(warm.rgba, studio_frame);
        let warm_frame = warm.rgba.clone();
        warm.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        assert_eq!(warm.rgba, warm_frame);

        let mut cool = Viewport::new(200, 200);
        cool.set_lighting_settings(PbrLightingSettings {
            environment: EnvironmentPreset::Cool,
            ..PbrLightingSettings::default()
        });
        cool.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        assert_ne!(cool.rgba, warm_frame);
        assert_ne!(cool.rgba, studio_frame);
    }

    #[test]
    fn analytic_environment_specular_response_is_roughness_blurred() {
        let environment = AnalyticEnvironment::from_preset(EnvironmentPreset::Studio);
        let at_key = environment.key_direction;
        let smooth = environment.radiance(at_key, MIN_PERCEPTUAL_ROUGHNESS);
        let rough = environment.radiance(at_key, 1.0);
        assert!(smooth.is_finite() && rough.is_finite());
        assert!(smooth.max_element() > rough.max_element());
        assert_ne!(smooth, rough);

        let lighting = PbrLighting {
            direction_to_light: Vec3::ZERO,
            direct_radiance: Vec3::ZERO,
            ambient_irradiance: Vec3::ZERO,
            environment: Some(EnvironmentPreset::Studio),
        };
        let smooth_pixel = shade_pbr(
            analytic_surface(Vec3::new(0.8, 0.4, 0.2), 1.0, MIN_PERCEPTUAL_ROUGHNESS),
            at_key,
            at_key,
            lighting,
        );
        let rough_pixel = shade_pbr(
            analytic_surface(Vec3::new(0.8, 0.4, 0.2), 1.0, 1.0),
            at_key,
            at_key,
            lighting,
        );
        assert!(smooth_pixel.is_finite() && rough_pixel.is_finite());
        assert!(smooth_pixel.max_element() > rough_pixel.max_element());
    }

    #[test]
    fn changing_environment_invalidates_incremental_cache_and_full_render_is_stable() {
        let doc = Document::new(64);
        let library = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        view.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        view.set_lighting_settings(PbrLightingSettings {
            environment: EnvironmentPreset::Cool,
            ..PbrLightingSettings::default()
        });
        assert_eq!(
            view.refresh_material_tiles(
                &library,
                &doc.material_bindings,
                doc.active_set_id,
                Channel::BaseColor,
                16,
                &[(0, 0, 16, 16)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
        view.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        let expected = view.rgba.clone();
        let mut fresh = Viewport::new(200, 200);
        fresh.set_lighting_settings(view.lighting_settings());
        fresh.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        assert_eq!(view.rgba, expected);
        assert_eq!(view.rgba, fresh.rgba);
    }

    #[test]
    fn incremental_material_tile_refresh_matches_full_render_byte_for_byte() {
        for channel in Channel::ALL {
            let mut doc = Document::new(64);
            let mut library = MaterialLibrary::from_document(&doc);
            let mut incremental = Viewport::new(200, 200);
            incremental.render_bound(&doc.mesh, &library, &doc.material_bindings, None);

            let hit = incremental
                .surface_hits
                .iter()
                .flatten()
                .next()
                .expect("reference mesh covers viewport pixels")
                .pick;
            let uv = Vec2::from_array(hit.uv).clamp(Vec2::ZERO, Vec2::ONE);
            let tile_size = 16_u32;
            let texel_x = (uv.x * (doc.size - 1) as f32).round() as u32;
            let texel_y = (uv.y * (doc.size - 1) as f32).round() as u32;
            let tile_x = texel_x / tile_size * tile_size;
            let tile_y = texel_y / tile_size * tile_size;
            let tile_width = tile_size.min(doc.size - tile_x);
            let tile_height = tile_size.min(doc.size - tile_y);
            let size = doc.size;
            let pixels = doc.selected_layer_mut().unwrap().pixels_mut(channel, size);
            for y in tile_y..tile_y + tile_height {
                for x in tile_x..tile_x + tile_width {
                    pixels.put_pixel(x, y, image::Rgba([220, 35, 80, 255]));
                }
            }
            let region = doc
                .composite_region_for_set(
                    doc.active_set_id,
                    channel,
                    tile_x,
                    tile_y,
                    tile_width,
                    tile_height,
                )
                .expect("updated tile can be composited");
            assert!(library.patch_region(doc.active_set_id, channel, tile_x, tile_y, &region,));

            let refreshed = incremental.refresh_material_tiles(
                &library,
                &doc.material_bindings,
                doc.active_set_id,
                channel,
                tile_size,
                &[(tile_x, tile_y, tile_width, tile_height)],
                None,
            );
            if channel == Channel::Opacity {
                assert_eq!(refreshed, ScreenRefresh::CacheInvalid);
                incremental.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
            } else {
                assert!(matches!(refreshed, ScreenRefresh::Updated(_)));
            }

            let mut full = Viewport::new(200, 200);
            full.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
            assert_eq!(incremental.rgba, full.rgba);
            assert_eq!(incremental.material_hits, full.material_hits);
            assert_eq!(incremental.uv, full.uv);
        }
    }

    #[test]
    fn incremental_material_refresh_matches_full_render_for_many_changed_tiles() {
        let mut doc = Document::new(64);
        let mut library = MaterialLibrary::from_document(&doc);
        let mut incremental = Viewport::new(200, 200);
        incremental.render_bound(&doc.mesh, &library, &doc.material_bindings, None);

        let tile_size = 8_u32;
        let size = doc.size;
        let mut tiles = Vec::new();
        for tile_y in (0..size).step_by(tile_size as usize) {
            for tile_x in (0..size).step_by(tile_size as usize) {
                let tile_width = tile_size.min(size - tile_x);
                let tile_height = tile_size.min(size - tile_y);
                let color = image::Rgba([
                    (tile_x * 3 + tile_y) as u8,
                    (tile_y * 5 + 17) as u8,
                    (tile_x * 7 + 31) as u8,
                    255,
                ]);
                let pixels = doc
                    .selected_layer_mut()
                    .unwrap()
                    .pixels_mut(Channel::BaseColor, size);
                for y in tile_y..tile_y + tile_height {
                    for x in tile_x..tile_x + tile_width {
                        pixels.put_pixel(x, y, color);
                    }
                }
                let region = doc
                    .composite_region_for_set(
                        doc.active_set_id,
                        Channel::BaseColor,
                        tile_x,
                        tile_y,
                        tile_width,
                        tile_height,
                    )
                    .expect("changed tile can be composited");
                assert!(library.patch_region(
                    doc.active_set_id,
                    Channel::BaseColor,
                    tile_x,
                    tile_y,
                    &region,
                ));
                tiles.push((tile_x, tile_y, tile_width, tile_height));
            }
        }

        assert_eq!(tiles.len(), 64);
        let refreshed = incremental.refresh_material_tiles(
            &library,
            &doc.material_bindings,
            doc.active_set_id,
            Channel::BaseColor,
            tile_size,
            &tiles,
            None,
        );
        assert!(matches!(refreshed, ScreenRefresh::Updated(_)));

        let mut full = Viewport::new(200, 200);
        full.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        assert_eq!(incremental.rgba, full.rgba);
        assert_eq!(incremental.material_hits, full.material_hits);
        assert_eq!(incremental.uv, full.uv);
    }

    #[test]
    fn material_tile_refresh_returns_separate_damage_for_disjoint_visible_regions() {
        let mut doc = Document::new(64);
        let mut library = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        let depth = 3.5;
        let screen_triangles = [
            [[20.5, 65.5], [78.5, 65.5], [50.5, 130.5]],
            [[120.5, 65.5], [178.5, 65.5], [150.5, 130.5]],
        ];
        let mut mesh = Mesh {
            name: "disjoint damage regions".into(),
            vertices: Vec::new(),
            indices: Vec::new(),
            materials: vec!["single".into()],
            triangle_materials: Vec::new(),
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        };
        for (triangle_index, screen) in screen_triangles.into_iter().enumerate() {
            let base = mesh.vertices.len() as u32;
            let uv = if triangle_index == 0 {
                [0.1, 0.1]
            } else {
                [0.9, 0.9]
            };
            for point in screen {
                mesh.vertices.push(crate::mesh::Vertex {
                    position: screen_position(&view, point[0], point[1], depth),
                    normal: [0.0, 0.0, 1.0],
                    uv,
                    tangent: None,
                    extensions: Default::default(),
                });
            }
            mesh.indices.extend([base, base + 1, base + 2]);
            mesh.triangle_materials.push(0);
        }
        view.render_bound(&mesh, &library, &[], None);
        let original_frame = view.rgba.clone();
        let changed_tiles = [(0_u32, 0_u32), (56_u32, 56_u32)];
        let changed_tile_cells = [(0_u32, 0_u32), (7_u32, 7_u32)];
        let mut affected_pixels = vec![false; view.surface_hits.len()];
        for (index, surface) in view.surface_hits.iter().enumerate() {
            let Some(surface) = surface else { continue };
            let texel_x = (surface.pick.uv[0].clamp(0.0, 1.0) * 63.0).round() as u32;
            let texel_y = (surface.pick.uv[1].clamp(0.0, 1.0) * 63.0).round() as u32;
            let tile_cell = (texel_x / 8, texel_y / 8);
            affected_pixels[index] = changed_tile_cells.contains(&tile_cell);
        }
        assert!(affected_pixels.iter().any(|affected| *affected));

        for (tile, color) in changed_tiles.into_iter().zip([
            image::Rgba([250, 20, 20, 255]),
            image::Rgba([20, 250, 20, 255]),
        ]) {
            let pixels = doc
                .selected_layer_mut()
                .unwrap()
                .pixels_mut(Channel::BaseColor, 64);
            for y in tile.1..tile.1 + 8 {
                for x in tile.0..tile.0 + 8 {
                    pixels.put_pixel(x, y, color);
                }
            }
            let region = doc
                .composite_region_for_set(
                    doc.active_set_id,
                    Channel::BaseColor,
                    tile.0,
                    tile.1,
                    8,
                    8,
                )
                .expect("edited tile should composite");
            assert!(library.patch_region(
                doc.active_set_id,
                Channel::BaseColor,
                tile.0,
                tile.1,
                &region,
            ));
        }

        let ScreenRefresh::Updated(rects) = view.refresh_material_tiles(
            &library,
            &[],
            doc.active_set_id,
            Channel::BaseColor,
            8,
            &[(0, 0, 8, 8), (56, 56, 8, 8)],
            None,
        ) else {
            panic!("both changed UV regions should remain visible");
        };
        assert_eq!(
            rects.len(),
            2,
            "disjoint screen regions should stay separate"
        );
        assert!(rects
            .iter()
            .all(|rect| { rect.width <= 80 && rect.height <= 80 }));

        let mut changed_pixels_per_region = [0_usize; 2];
        for (index, affected) in affected_pixels.iter().copied().enumerate() {
            let x = index % view.width;
            let y = index / view.width;
            let offset = index * 4;
            let changed = original_frame[offset..offset + 4] != view.rgba[offset..offset + 4];
            if affected {
                assert!(
                    rects.iter().any(|rect| {
                        x >= rect.x
                            && x < rect.x + rect.width
                            && y >= rect.y
                            && y < rect.y + rect.height
                    }),
                    "damage rectangles must cover changed pixel ({x}, {y})"
                );
                if changed {
                    let region = if x < view.width / 2 { 0 } else { 1 };
                    changed_pixels_per_region[region] += 1;
                }
            } else {
                assert_eq!(
                    &original_frame[offset..offset + 4],
                    &view.rgba[offset..offset + 4],
                    "unrelated screen pixels must remain byte-identical"
                );
            }
        }
        assert!(changed_pixels_per_region.iter().all(|count| *count > 0));
        let mut full = Viewport::new(200, 200);
        full.render_bound(&mesh, &library, &[], None);
        assert_eq!(view.rgba, full.rgba);
    }

    #[test]
    fn material_tile_refresh_rejects_more_than_screen_damage_rect_cap_without_mutation() {
        let mut doc = Document::new(64);
        let mut library = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(400, 400);
        let mesh = triangle_mesh(&view, 3.5, [[0.1; 2]; 3], 0);
        view.render_bound(&mesh, &library, &[], None);

        let changed_surface = ScreenSurface {
            pick: PickHit {
                uv: [0.1, 0.1],
                material_slot: 0,
                triangle_index: 0,
                barycentric: [1.0, 0.0, 0.0],
            },
            position: Vec3::ZERO,
            normal: Vec3::Z,
            tangent: Vec3::X,
            tangent_handedness: 1.0,
        };
        view.surface_hits.fill(None);
        let mut changed_samples = 0;
        'cells: for cell_y in (0..view.height / SCREEN_DAMAGE_CELL_SIZE).step_by(2) {
            for cell_x in (0..view.width / SCREEN_DAMAGE_CELL_SIZE).step_by(2) {
                let x = cell_x * SCREEN_DAMAGE_CELL_SIZE + SCREEN_DAMAGE_CELL_SIZE / 2;
                let y = cell_y * SCREEN_DAMAGE_CELL_SIZE + SCREEN_DAMAGE_CELL_SIZE / 2;
                view.surface_hits[y * view.width + x] = Some(changed_surface);
                changed_samples += 1;
                if changed_samples > MAX_SCREEN_DAMAGE_RECTS {
                    break 'cells;
                }
            }
        }
        assert_eq!(changed_samples, MAX_SCREEN_DAMAGE_RECTS + 1);

        let pixels = doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64);
        for y in 0..8 {
            for x in 0..8 {
                pixels.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
        }
        let region = doc
            .composite_region_for_set(doc.active_set_id, Channel::BaseColor, 0, 0, 8, 8)
            .expect("edited tile should composite");
        assert!(library.patch_region(doc.active_set_id, Channel::BaseColor, 0, 0, &region,));

        let original_frame = view.rgba.clone();
        assert_eq!(
            view.refresh_material_tiles(
                &library,
                &[],
                doc.active_set_id,
                Channel::BaseColor,
                8,
                &[(0, 0, 8, 8)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
        assert_eq!(view.rgba, original_frame);
    }

    #[test]
    fn screen_damage_coalescer_merges_edge_adjacent_cells() {
        let mut damaged = vec![false; 4 * 4];
        damaged[0] = true;
        damaged[1] = true;
        damaged[4] = true;
        damaged[15] = true;

        let rects = coalesce_screen_damage_cells(&damaged, 64, 64)
            .expect("two connected damage components fit the rectangle cap");
        assert_eq!(
            rects,
            vec![
                ScreenRect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 32,
                },
                ScreenRect {
                    x: 48,
                    y: 48,
                    width: 16,
                    height: 16,
                },
            ]
        );
    }

    #[test]
    fn invalid_material_tile_list_leaves_cached_pixels_unchanged() {
        let mut doc = Document::new(64);
        let size = doc.size;
        let mut library = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        view.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        let original_frame = view.rgba.clone();
        let hit = view
            .surface_hits
            .iter()
            .flatten()
            .next()
            .expect("reference mesh covers viewport pixels")
            .pick;
        let uv = Vec2::from_array(hit.uv).clamp(Vec2::ZERO, Vec2::ONE);
        let tile_size = 16_u32;
        let texel_x = (uv.x * (size - 1) as f32).round() as u32;
        let texel_y = (uv.y * (size - 1) as f32).round() as u32;
        let tile_x = texel_x / tile_size * tile_size;
        let tile_y = texel_y / tile_size * tile_size;
        let tile_width = tile_size.min(size - tile_x);
        let tile_height = tile_size.min(size - tile_y);
        let pixels = doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, size);
        for y in tile_y..tile_y + tile_height {
            for x in tile_x..tile_x + tile_width {
                pixels.put_pixel(x, y, image::Rgba([245, 9, 177, 255]));
            }
        }
        let region = doc
            .composite_region_for_set(
                doc.active_set_id,
                Channel::BaseColor,
                tile_x,
                tile_y,
                tile_width,
                tile_height,
            )
            .expect("changed tile can be composited");
        assert!(library.patch_region(
            doc.active_set_id,
            Channel::BaseColor,
            tile_x,
            tile_y,
            &region,
        ));

        assert_eq!(
            view.refresh_material_tiles(
                &library,
                &doc.material_bindings,
                doc.active_set_id,
                Channel::BaseColor,
                tile_size,
                &[(tile_x, tile_y, tile_width, tile_height), (64, 0, 1, 1)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
        assert_eq!(view.rgba, original_frame);
    }

    #[test]
    fn incremental_material_refresh_rejects_invalidated_camera_cache() {
        let doc = Document::new(64);
        let library = MaterialLibrary::from_document(&doc);
        let mut view = Viewport::new(200, 200);
        view.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
        view.zoom(0.5);
        assert_eq!(
            view.refresh_material_tiles(
                &library,
                &doc.material_bindings,
                doc.active_set_id,
                Channel::BaseColor,
                16,
                &[(0, 0, 16, 16)],
                None,
            ),
            ScreenRefresh::CacheInvalid
        );
    }

    #[test]
    #[ignore = "manual CPU paint-refresh benchmark; run in release mode with --ignored --nocapture"]
    fn benchmark_paint_refresh_at_1k_2k_and_4k_texture_resolutions() {
        fn median_millis(samples: &mut [std::time::Duration]) -> f64 {
            samples.sort_unstable();
            samples[samples.len() / 2].as_secs_f64() * 1000.0
        }

        println!("texture,composite_tile_ms,incremental_view_ms,full_view_ms");
        for resolution in [1024_u32, 2048, 4096] {
            let mut doc = Document::new(resolution);
            let mut library = MaterialLibrary::from_document(&doc);
            let mut incremental = Viewport::new(512, 512);
            incremental.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
            let hit = incremental
                .surface_hits
                .iter()
                .flatten()
                .next()
                .expect("reference mesh covers viewport pixels")
                .pick;
            let uv = Vec2::from_array(hit.uv).clamp(Vec2::ZERO, Vec2::ONE);
            let texel_x = (uv.x * (resolution - 1) as f32).round() as u32;
            let texel_y = (uv.y * (resolution - 1) as f32).round() as u32;
            let tile_size = 64_u32;
            let tile_x = texel_x / tile_size * tile_size;
            let tile_y = texel_y / tile_size * tile_size;
            let width = tile_size.min(resolution - tile_x);
            let height = tile_size.min(resolution - tile_y);
            let mut full = Viewport::new(512, 512);
            let mut composite_samples = Vec::new();
            let mut incremental_samples = Vec::new();
            let mut full_samples = Vec::new();

            for sample in 0..7_u8 {
                let edit_start = std::time::Instant::now();
                let pixels = doc
                    .selected_layer_mut()
                    .unwrap()
                    .pixels_mut(Channel::BaseColor, resolution);
                for y in tile_y..tile_y + height {
                    for x in tile_x..tile_x + width {
                        pixels.put_pixel(x, y, image::Rgba([sample, 80, 150, 255]));
                    }
                }
                let region = doc
                    .composite_region_for_set(
                        doc.active_set_id,
                        Channel::BaseColor,
                        tile_x,
                        tile_y,
                        width,
                        height,
                    )
                    .expect("paint tile composites");
                composite_samples.push(edit_start.elapsed());
                assert!(library.patch_region(
                    doc.active_set_id,
                    Channel::BaseColor,
                    tile_x,
                    tile_y,
                    &region,
                ));

                let incremental_start = std::time::Instant::now();
                let refresh = incremental.refresh_material_tiles(
                    &library,
                    &doc.material_bindings,
                    doc.active_set_id,
                    Channel::BaseColor,
                    tile_size,
                    &[(tile_x, tile_y, width, height)],
                    None,
                );
                incremental_samples.push(incremental_start.elapsed());
                assert!(matches!(refresh, ScreenRefresh::Updated(_)));

                let full_start = std::time::Instant::now();
                full.render_bound(&doc.mesh, &library, &doc.material_bindings, None);
                full_samples.push(full_start.elapsed());
                assert_eq!(incremental.rgba, full.rgba);
            }
            println!(
                "{resolution},{:.3},{:.3},{:.3}",
                median_millis(&mut composite_samples),
                median_millis(&mut incremental_samples),
                median_millis(&mut full_samples),
            );
        }
    }

    #[test]
    fn edited_material_map_changes_the_next_reference_frame() {
        let mut doc = Document::new(64);
        let mesh = doc.mesh.clone();
        let mut viewport = Viewport::new(200, 200);

        let initial_material = Material::from_document(&doc);
        viewport.render(&mesh, &initial_material, None);
        let initial_frame = viewport.rgba.clone();

        let base_color = doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::BaseColor, 64);
        for pixel in base_color.pixels_mut() {
            *pixel = image::Rgba([240, 24, 12, 255]);
        }

        // Material is a composited snapshot. Rebuilding it after a committed
        // document edit must make that edit visible in the following frame.
        let updated_material = Material::from_document(&doc);
        viewport.render(&mesh, &updated_material, None);
        assert_ne!(viewport.rgba, initial_frame);
    }

    #[test]
    fn analytic_triangle_maps_screen_pixel_to_expected_uv_and_material() {
        let view = Viewport::new(200, 200);
        let mesh = triangle_mesh(&view, 3.5, [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]], 1);
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);

        // Pixel (100, 100) is sampled at (100.5, 100.5), whose barycentric
        // weights in this screen-space triangle are exactly [0.25, 0.25, 0.5].
        let hit = rendered
            .pick_surface(100, 100)
            .expect("triangle covers sample pixel");
        let uv = hit.uv;
        assert!((uv[0] - 0.35).abs() < 0.002, "unexpected U: {}", uv[0]);
        assert!((uv[1] - 0.50).abs() < 0.002, "unexpected V: {}", uv[1]);
        assert_eq!(rendered.pick_material_slot(100, 100), Some(1));
        assert_eq!(hit.material_slot, 1);
        assert_eq!(hit.triangle_index, 0);
        for (actual, expected) in hit.barycentric.iter().zip([0.25, 0.25, 0.5]) {
            assert!((actual - expected).abs() < 0.002);
        }
    }

    #[test]
    fn transformed_triangle_picking_matches_world_space_ray_oracle() {
        let view = Viewport::new(240, 200);
        let mut mesh = triangle_mesh(&view, 3.5, [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]], 1);
        let transform = glam::Mat4::from_translation(Vec3::new(0.12, -0.08, 0.1))
            * glam::Mat4::from_rotation_y(0.16)
            * glam::Mat4::from_scale(Vec3::new(1.08, 0.92, 1.0));
        let normal_transform = transform.inverse().transpose();
        for vertex in &mut mesh.vertices {
            vertex.position = transform
                .transform_point3(Vec3::from_array(vertex.position))
                .to_array();
            vertex.normal = normal_transform
                .transform_vector3(Vec3::from_array(vertex.normal))
                .normalize_or_zero()
                .to_array();
        }

        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);
        let (index, actual_uv) = rendered
            .uv
            .iter()
            .enumerate()
            .find_map(|(index, uv)| uv.map(|uv| (index, uv)))
            .expect("transformed triangle should cover visible samples");
        let (x, y) = (index % rendered.width, index / rendered.width);
        let expected_uv = ray_intersection_uv(&rendered, &mesh, x, y)
            .expect("world-space ray should intersect the transformed triangle");
        assert!((actual_uv[0] - expected_uv[0]).abs() < 0.002);
        assert!((actual_uv[1] - expected_uv[1]).abs() < 0.002);
        assert_eq!(rendered.pick_material_slot(x, y), Some(1));
    }

    #[test]
    fn overlapping_triangles_pick_nearest_uv_and_material() {
        let view = Viewport::new(200, 200);
        let mut mesh = triangle_mesh(&view, 4.5, [[0.7, 0.7], [0.9, 0.7], [0.7, 0.9]], 0);
        let front = triangle_mesh(&view, 2.5, [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]], 1);
        mesh.vertices.extend(front.vertices);
        mesh.indices.extend([3, 4, 5]);
        mesh.triangle_materials.push(1);

        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);

        let hit = rendered
            .pick_surface(100, 100)
            .expect("overlapping triangle covers sample");
        let uv = hit.uv;
        assert!(
            (uv[0] - 0.35).abs() < 0.002,
            "unexpected picked U: {}",
            uv[0]
        );
        assert!(
            (uv[1] - 0.50).abs() < 0.002,
            "unexpected picked V: {}",
            uv[1]
        );
        assert_eq!(rendered.pick_material_slot(100, 100), Some(1));
        assert_eq!(hit.material_slot, 1);
        assert_eq!(hit.triangle_index, 1);
        for (actual, expected) in hit.barycentric.iter().zip([0.25, 0.25, 0.5]) {
            assert!((actual - expected).abs() < 0.002);
        }
    }

    #[test]
    fn mirrored_uv_normal_map_preserves_world_space_bitangent() {
        let view = Viewport::new(200, 200);
        let mut doc = Document::new(64);
        for pixel in doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, 64)
            .chunks_exact_mut(4)
        {
            pixel.copy_from_slice(&[128, 255, 128, 255]);
        }
        let material = Material::from_document(&doc);
        let mut regular = triangle_mesh(&view, 3.5, [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]], 0);
        let mut mirrored = triangle_mesh(&view, 3.5, [[0.9, 0.2], [0.1, 0.2], [0.8, 0.8]], 0);
        let eye = Vec3::new(
            view.distance * view.yaw.sin() * view.pitch.cos(),
            view.distance * view.pitch.sin(),
            view.distance * view.yaw.cos(),
        );
        let face_normal = eye.normalize();
        for vertex in regular.vertices.iter_mut().chain(&mut mirrored.vertices) {
            vertex.normal = face_normal.to_array();
        }

        let mut regular_view = Viewport::new(200, 200);
        regular_view.render(&regular, &material, None);
        let mut mirrored_view = Viewport::new(200, 200);
        mirrored_view.render(&mirrored, &material, None);
        let pixel_offset = (100 * view.width + 100) * 4;
        assert_eq!(
            &regular_view.rgba[pixel_offset..pixel_offset + 3],
            &mirrored_view.rgba[pixel_offset..pixel_offset + 3],
            "a +V normal should shade the same world direction across mirrored UVs"
        );
    }

    fn material_with_normal(normal: [u8; 3]) -> Material {
        let mut doc = Document::new(64);
        for pixel in doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Normal, 64)
            .chunks_exact_mut(4)
        {
            pixel.copy_from_slice(&[normal[0], normal[1], normal[2], 255]);
        }
        for pixel in doc
            .selected_layer_mut()
            .unwrap()
            .pixels_mut(Channel::Roughness, 64)
            .chunks_exact_mut(4)
        {
            pixel.copy_from_slice(&[255, 255, 255, 255]);
        }
        Material::from_document(&doc)
    }

    fn normal_facing_camera(view: &Viewport) -> Vec3 {
        Vec3::new(
            view.distance * view.yaw.sin() * view.pitch.cos(),
            view.distance * view.pitch.sin(),
            view.distance * view.yaw.cos(),
        )
        .normalize()
    }

    fn render_center_with_tangents(
        view: &Viewport,
        material: &Material,
        tangents: [Option<[f32; 4]>; 3],
        lighting_direction: Vec3,
    ) -> [u8; 3] {
        let normal = normal_facing_camera(view);
        let mut mesh = triangle_mesh(view, 3.5, [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]], 0);
        for (vertex, tangent) in mesh.vertices.iter_mut().zip(tangents) {
            vertex.normal = normal.to_array();
            vertex.tangent = tangent;
        }
        let lighting_direction = lighting_direction.normalize();
        let mut rendered = Viewport::new(view.width, view.height);
        rendered.lighting = PbrLightingSettings {
            key_azimuth_degrees: lighting_direction
                .x
                .atan2(lighting_direction.z)
                .to_degrees(),
            key_elevation_degrees: lighting_direction.y.asin().to_degrees(),
            key_intensity: 4.0,
            ambient_fill: 0.05,
            environment: EnvironmentPreset::Studio,
        };
        rendered.render(&mesh, material, None);
        let offset = (100 * rendered.width + 100) * 4;
        rendered.rgba[offset..offset + 3].try_into().unwrap()
    }

    fn pixel_brightness(pixel: [u8; 3]) -> u16 {
        u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2])
    }

    #[test]
    fn supplied_vertex_tangents_change_tangent_normal_map_shading() {
        let view = Viewport::new(200, 200);
        let normal = normal_facing_camera(&view);
        let tangent = (Vec3::X - normal * normal.dot(Vec3::X)).normalize();
        let bitangent = normal.cross(tangent).normalize();
        let material = material_with_normal([255, 128, 128]);
        let along_tangent = render_center_with_tangents(
            &view,
            &material,
            [Some(tangent.extend(1.0).to_array()); 3],
            tangent,
        );
        let along_bitangent = render_center_with_tangents(
            &view,
            &material,
            [Some(bitangent.extend(1.0).to_array()); 3],
            tangent,
        );

        assert_ne!(along_tangent, along_bitangent);
        assert!(
            pixel_brightness(along_tangent) > pixel_brightness(along_bitangent),
            "a +T normal should face the key light when T follows the key: {along_tangent:?} vs {along_bitangent:?}"
        );
    }

    #[test]
    fn supplied_tangent_handedness_flips_the_bitangent_normal_response() {
        let view = Viewport::new(200, 200);
        let normal = normal_facing_camera(&view);
        let tangent = (Vec3::X - normal * normal.dot(Vec3::X)).normalize();
        let bitangent = normal.cross(tangent).normalize();
        let material = material_with_normal([128, 255, 128]);
        let positive_handedness = render_center_with_tangents(
            &view,
            &material,
            [Some(tangent.extend(1.0).to_array()); 3],
            bitangent,
        );
        let negative_handedness = render_center_with_tangents(
            &view,
            &material,
            [Some(tangent.extend(-1.0).to_array()); 3],
            bitangent,
        );

        assert_ne!(positive_handedness, negative_handedness);
        assert!(
            pixel_brightness(positive_handedness) > pixel_brightness(negative_handedness),
            "positive handedness should map +B toward the key: {positive_handedness:?} vs {negative_handedness:?}"
        );
    }

    #[test]
    fn any_missing_vertex_tangent_uses_the_uv_derived_fallback() {
        let view = Viewport::new(200, 200);
        let normal = normal_facing_camera(&view);
        let tangent = (Vec3::X - normal * normal.dot(Vec3::X)).normalize();
        let material = material_with_normal([255, 128, 128]);
        let no_tangents = render_center_with_tangents(&view, &material, [None; 3], tangent);
        let one_missing = render_center_with_tangents(
            &view,
            &material,
            [Some(tangent.extend(1.0).to_array()), None, None],
            tangent,
        );

        assert_eq!(one_missing, no_tangents);
    }

    #[test]
    fn degenerate_triangle_produces_no_picking_hits() {
        let view = Viewport::new(200, 200);
        let mut mesh = triangle_mesh(&view, 3.5, [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]], 0);
        let position = mesh.vertices[0].position;
        for vertex in &mut mesh.vertices {
            vertex.position = position;
        }
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);

        assert!(rendered.uv.iter().all(Option::is_none));
        assert!(rendered.material_hits.iter().all(Option::is_none));
    }

    #[test]
    fn triangle_crossing_near_plane_with_one_vertex_behind_remains_pickable() {
        let view = Viewport::new(200, 200);
        let mesh = triangle_mesh_with_depths(
            &view,
            [-0.5, 3.5, 3.5],
            [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]],
            1,
        );
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);

        let (index, hit) = rendered
            .uv
            .iter()
            .enumerate()
            .find_map(|(index, uv)| {
                uv.map(|_| {
                    (
                        index,
                        rendered
                            .pick_surface(index % rendered.width, index / rendered.width)
                            .unwrap(),
                    )
                })
            })
            .expect("clipped triangle remains pickable");
        let (x, y) = (index % rendered.width, index / rendered.width);
        let expected = ray_intersection_barycentric(&rendered, &mesh, x, y).unwrap_or_else(|| {
            panic!(
                "picked sample ray at ({x}, {y}) misses source triangle for UV {:?}",
                hit.uv
            )
        });
        assert_eq!(hit.triangle_index, 0);
        assert_eq!(hit.material_slot, 1);
        for (actual, expected) in hit.barycentric.iter().zip(expected) {
            assert!((actual - expected).abs() < 0.002);
        }
        assert!(rendered.material_hits.contains(&Some(1)));
        assert_eq!(rendered.pick_material_slot(x, y), Some(1));
    }

    #[test]
    fn triangle_crossing_near_plane_with_two_vertices_behind_remains_pickable() {
        let view = Viewport::new(200, 200);
        let mesh = triangle_mesh_with_depths(
            &view,
            [3.5, -0.5, -0.5],
            [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]],
            1,
        );
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);

        let (index, hit) = rendered
            .uv
            .iter()
            .enumerate()
            .find_map(|(index, uv)| {
                uv.map(|_| {
                    (
                        index,
                        rendered
                            .pick_surface(index % rendered.width, index / rendered.width)
                            .unwrap(),
                    )
                })
            })
            .expect("clipped triangle remains pickable");
        let (x, y) = (index % rendered.width, index / rendered.width);
        let expected = ray_intersection_barycentric(&rendered, &mesh, x, y)
            .expect("the picked sample ray intersects the source triangle");
        assert_eq!(hit.triangle_index, 0);
        assert_eq!(hit.material_slot, 1);
        for (actual, expected) in hit.barycentric.iter().zip(expected) {
            assert!((actual - expected).abs() < 0.002);
        }
        assert!(rendered.material_hits.contains(&Some(1)));
        assert_eq!(rendered.pick_material_slot(x, y), Some(1));
    }

    #[test]
    fn fully_behind_triangle_produces_no_picking_hits() {
        let view = Viewport::new(200, 200);
        let mesh = triangle_mesh_with_depths(
            &view,
            [-1.5, -0.5, -2.0],
            [[0.1, 0.2], [0.9, 0.2], [0.2, 0.8]],
            1,
        );
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut rendered = view;
        rendered.render(&mesh, &material, None);

        assert!(rendered.uv.iter().all(Option::is_none));
        assert!(rendered.material_hits.iter().all(Option::is_none));
    }
}

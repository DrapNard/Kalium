//! Bounded, deterministic CPU mesh-map baking primitives.
//!
//! This first slice rasterizes the active mesh's own attributes into a
//! Texture Set's UV atlas. It does not perform high-to-low ray projection and
//! its outputs are not yet part of the persisted document schema.
use crate::mesh::Mesh;
use anyhow::{bail, Context, Result};
use glam::{DVec2, DVec3, Vec3};
use image::{Rgba, RgbaImage};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_BAKE_SIZE: u32 = 4096;
const MAX_RASTER_SAMPLES: u64 = 90_000_000;
const MAX_AO_SAMPLES_PER_TEXEL: u32 = 4096;
const MAX_AO_RAYS: u64 = 100_000_000;
const BVH_NODE_LEAF_TRIANGLES: usize = 8;
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
    pub fn is_valid(&self, x: u32, y: u32) -> bool {
        x < self.size && y < self.size && self.valid[(y * self.size + x) as usize] != 0
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
/// triangles can occlude. This is a CPU primitive with no GUI or persisted
/// document integration.
pub fn bake_ambient_occlusion(
    mesh: &Mesh,
    options: AmbientOcclusionBakeOptions<'_>,
    cancel: &AtomicBool,
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
    }

    Ok(BakedAmbientOcclusion {
        size: options.size,
        occlusion,
        valid: maps.valid,
    })
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
    use std::sync::atomic::AtomicBool;

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
            extensions: Map::new(),
        }
    }

    fn vertex(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> Vertex {
        Vertex {
            position,
            normal,
            uv,
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
}

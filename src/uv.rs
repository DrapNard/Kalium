//! Deterministic, bounded CPU UV coverage rasterizer.
//!
//! The result is a one-byte-per-texel mask. It is independent of the UI and
//! used to prevent brush stamps and flood fills from modifying unoccupied UV
//! atlas space. It is NOT an island ID map and does not make UV seams seamless.
use crate::mesh::Mesh;
use anyhow::{bail, Result};
use glam::DVec2;
use image::{GrayImage, Luma};

const MAX_RASTER_SAMPLES: u64 = 90_000_000;
#[allow(dead_code)] // S11 addressing groundwork; editor paths remain single-tile.
const MIN_UDIM: u32 = 1001;
#[allow(dead_code)] // S11 addressing groundwork; editor paths remain single-tile.
const MAX_UDIM: u32 = 1999;

/// Standard UDIM tile coordinates. U and V follow the source mesh UV axes;
/// local V remains in Kalium's existing texture-space row convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UvTileCoord {
    pub u: i32,
    pub v: i32,
}

impl UvTileCoord {
    #[allow(dead_code)] // Used by tile-coverage groundwork until editor integration.
    pub fn udim_id(self) -> Option<u32> {
        if self.u < 0 || self.v < 0 || self.u > 9 || self.v > 99 {
            return None;
        }
        let id = MIN_UDIM + self.u as u32 + 10 * self.v as u32;
        (id <= MAX_UDIM).then_some(id)
    }
}

/// Resolve mesh UV coordinates into an accepted standard UDIM and its local
/// `[0, 1)` coordinates. Exact integer borders belong to the tile on the
/// positive U/V side; negative and nonstandard tile coordinates are rejected.
#[allow(dead_code)] // S11 groundwork; current paint paths still address one tile.
pub fn split_udim_uv(uv: [f32; 2]) -> Option<(UvTileCoord, [f32; 2])> {
    if uv.iter().any(|value| !value.is_finite() || *value < 0.0) {
        return None;
    }
    let tile_u = uv[0].floor();
    let tile_v = uv[1].floor();
    if tile_u > 9.0 || tile_v > 99.0 {
        return None;
    }
    let tile = UvTileCoord {
        u: tile_u as i32,
        v: tile_v as i32,
    };
    tile.udim_id()?;
    Some((tile, [uv[0] - tile_u, uv[1] - tile_v]))
}

#[derive(Clone)]
pub struct UvCoverage {
    pub size: u32,
    cells: Vec<u8>,
    covered: usize,
}

impl UvCoverage {
    pub fn generate(mesh: &Mesh, size: u32) -> Result<Self> {
        if !(64..=4096).contains(&size) {
            bail!("UV coverage dimension must be between 64 and 4096");
        }
        Self::generate_filtered(mesh, size, None, None)
    }
    /// Only UVs of triangles assigned to the selected Texture Set are paintable.
    pub fn generate_for_set(
        mesh: &Mesh,
        size: u32,
        bindings: &[Option<u32>],
        active_id: u32,
    ) -> Result<Self> {
        Self::generate_filtered(mesh, size, Some((bindings, active_id)), None)
    }
    /// Rasterize only the selected tile, keeping material Texture Set identity
    /// separate from the UDIM tile coordinate.
    #[allow(dead_code)] // S11 groundwork; storage and paint routing are pending.
    pub fn generate_for_tile(mesh: &Mesh, size: u32, tile: UvTileCoord) -> Result<Self> {
        if tile.udim_id().is_none() {
            bail!("UDIM tile coordinates are outside the supported range");
        }
        Self::generate_filtered(mesh, size, None, Some(tile))
    }
    #[allow(dead_code)] // S11 groundwork; storage and paint routing are pending.
    pub fn generate_for_tile_and_set(
        mesh: &Mesh,
        size: u32,
        bindings: &[Option<u32>],
        active_id: u32,
        tile: UvTileCoord,
    ) -> Result<Self> {
        if tile.udim_id().is_none() {
            bail!("UDIM tile coordinates are outside the supported range");
        }
        Self::generate_filtered(mesh, size, Some((bindings, active_id)), Some(tile))
    }
    fn generate_filtered(
        mesh: &Mesh,
        size: u32,
        filter: Option<(&[Option<u32>], u32)>,
        tile: Option<UvTileCoord>,
    ) -> Result<Self> {
        if !(64..=4096).contains(&size) {
            bail!("UV coverage dimension must be between 64 and 4096");
        }
        mesh.validate()?;
        let mut cells = vec![0u8; (size as usize) * (size as usize)];
        let edge = |p: DVec2, q: DVec2, r: DVec2| -> f64 {
            (r.x - p.x) * (q.y - p.y) - (r.y - p.y) * (q.x - p.x)
        };
        let mut raster_samples = 0u64;
        for (triangle_index, triangle) in mesh.indices.chunks_exact(3).enumerate() {
            if let Some((bindings, active_id)) = filter {
                let slot = mesh.triangle_material(triangle_index);
                let assigned = bindings.get(slot).copied().flatten().unwrap_or(active_id);
                if assigned != active_id {
                    continue;
                }
            }
            let pixel_uv = |index: u32| {
                let uv = mesh.vertices[index as usize].uv;
                let local = if let Some(tile) = tile {
                    [uv[0] - tile.u as f32, uv[1] - tile.v as f32]
                } else {
                    uv
                };
                DVec2::new(f64::from(local[0]), f64::from(local[1])) * f64::from(size - 1)
            };
            let a = pixel_uv(triangle[0]);
            let b = pixel_uv(triangle[1]);
            let c = pixel_uv(triangle[2]);
            let area = edge(a, b, c);
            if !area.is_finite() || area.abs() < 1e-7 {
                continue;
            }
            let scale = f64::from(size - 1);
            if a.max(b).max(c).x < 0.0
                || a.max(b).max(c).y < 0.0
                || a.min(b).min(c).x > scale
                || a.min(b).min(c).y > scale
            {
                continue;
            }
            let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as u32;
            let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as u32;
            let max_x = a.x.max(b.x).max(c.x).ceil().min(scale) as u32;
            let max_y = a.y.max(b.y).max(c.y).ceil().min(scale) as u32;
            if min_x > max_x || min_y > max_y {
                continue;
            }
            let count = (u64::from(max_x - min_x) + 1) * (u64::from(max_y - min_y) + 1);
            raster_samples = raster_samples.saturating_add(count);
            if raster_samples > MAX_RASTER_SAMPLES {
                bail!("UV coverage exceeds the CPU rasterization safety budget");
            }
            // Sample at actual texel centers used by Brush::stamp: integer
            // texel coordinates with UV = texel / (size-1).
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let p = DVec2::new(f64::from(x), f64::from(y));
                    let u = edge(b, c, p) / area;
                    let v = edge(c, a, p) / area;
                    let w = 1.0 - u - v;
                    if u.is_finite()
                        && v.is_finite()
                        && w.is_finite()
                        && u >= -1e-5
                        && v >= -1e-5
                        && w >= -1e-5
                    {
                        cells[(y as usize) * (size as usize) + x as usize] = 255;
                    }
                }
            }
        }
        let covered = cells.iter().filter(|p| **p != 0).count();
        Ok(Self {
            size,
            cells,
            covered,
        })
    }
    pub fn contains(&self, x: u32, y: u32) -> bool {
        x < self.size
            && y < self.size
            && self.cells[(y as usize) * (self.size as usize) + x as usize] != 0
    }
    pub fn contains_uv(&self, uv: [f32; 2]) -> bool {
        if uv
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
        {
            return false;
        }
        let x = (uv[0] * ((self.size - 1) as f32)).round() as u32;
        let y = (uv[1] * ((self.size - 1) as f32)).round() as u32;
        self.contains(x, y)
    }
    pub fn to_image(&self) -> GrayImage {
        GrayImage::from_raw(self.size, self.size, self.cells.clone())
            .unwrap_or_else(|| GrayImage::from_pixel(self.size, self.size, Luma([0])))
    }
    pub fn covered_texels(&self) -> usize {
        self.covered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::{Mesh, Vertex};
    fn small_triangle() -> Mesh {
        Mesh {
            name: "Triangle".into(),
            vertices: vec![
                Vertex {
                    position: [0., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.25, 0.25],
                    tangent: None,
                    extensions: Default::default(),
                },
                Vertex {
                    position: [1., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.75, 0.25],
                    tangent: None,
                    extensions: Default::default(),
                },
                Vertex {
                    position: [0., 1., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.25, 0.75],
                    tangent: None,
                    extensions: Default::default(),
                },
            ],
            indices: vec![0, 1, 2],
            materials: vec![],
            triangle_materials: vec![],
            extra_uv_sets: Default::default(),
            extensions: Default::default(),
        }
    }
    #[test]
    fn coverage_excludes_empty_uv_space() {
        let mask = UvCoverage::generate(&small_triangle(), 64).unwrap();
        assert!(mask.contains_uv([0.35, 0.35]));
        assert!(!mask.contains_uv([0.85, 0.85]));
        assert!(!mask.contains_uv([-0.1, 0.1]));
        assert!(mask.covered_texels() > 0);
    }
    #[test]
    fn reversed_winding_still_rasterizes() {
        let mut mesh = small_triangle();
        mesh.indices.swap(1, 2);
        let mask = UvCoverage::generate(&mesh, 64).unwrap();
        assert!(mask.contains_uv([0.35, 0.35]));
    }
    #[test]
    fn coverage_follows_texture_set_binding() {
        let mut mesh = Mesh::demo_cube();
        mesh.materials.push("Second".into());
        mesh.triangle_materials[0] = 1;
        let filtered = UvCoverage::generate_for_set(&mesh, 128, &[None, Some(2)], 1).unwrap();
        let full = UvCoverage::generate(&mesh, 128).unwrap();
        assert!(filtered.covered_texels() < full.covered_texels());
    }
    #[test]
    fn invalid_size_is_rejected() {
        assert!(UvCoverage::generate(&small_triangle(), 5000).is_err());
    }

    #[test]
    fn udim_addressing_uses_floor_and_positive_side_integer_boundaries() {
        assert_eq!(
            split_udim_uv([0.25, 0.75]),
            Some((UvTileCoord { u: 0, v: 0 }, [0.25, 0.75]))
        );
        assert_eq!(
            split_udim_uv([1.25, 0.75]),
            Some((UvTileCoord { u: 1, v: 0 }, [0.25, 0.75]))
        );
        assert_eq!(
            split_udim_uv([1.0, 1.0]),
            Some((UvTileCoord { u: 1, v: 1 }, [0.0, 0.0]))
        );
        assert_eq!(UvTileCoord { u: 0, v: 0 }.udim_id(), Some(1001));
        assert_eq!(UvTileCoord { u: 1, v: 0 }.udim_id(), Some(1002));
        assert_eq!(UvTileCoord { u: 0, v: 1 }.udim_id(), Some(1011));
        assert_eq!(UvTileCoord { u: 9, v: 99 }.udim_id(), None);
        assert_eq!(split_udim_uv([-0.01, 0.5]), None);
        assert_eq!(split_udim_uv([f32::NAN, 0.5]), None);
        assert_eq!(split_udim_uv([10.0, 0.5]), None);
        assert_eq!(split_udim_uv([0.5, 100.0]), None);
    }

    #[test]
    fn tile_coverage_clips_crossing_triangles_and_keeps_legacy_api() {
        let mut mesh = small_triangle();
        mesh.vertices[0].uv = [0.75, 0.25];
        mesh.vertices[1].uv = [1.25, 0.25];
        mesh.vertices[2].uv = [0.75, 0.75];
        let legacy = UvCoverage::generate(&mesh, 64).unwrap();
        let first_tile =
            UvCoverage::generate_for_tile(&mesh, 64, UvTileCoord { u: 0, v: 0 }).unwrap();
        let next_tile =
            UvCoverage::generate_for_tile(&mesh, 64, UvTileCoord { u: 1, v: 0 }).unwrap();
        assert!(legacy.covered_texels() > 0);
        assert!(first_tile.covered_texels() > 0);
        assert!(next_tile.covered_texels() > 0);
        assert!(first_tile.covered_texels() > next_tile.covered_texels());

        let local_mesh = {
            let mut local = mesh.clone();
            for vertex in &mut local.vertices {
                vertex.uv[0] -= 1.0;
            }
            local
        };
        assert_eq!(
            next_tile.to_image().as_raw(),
            UvCoverage::generate(&local_mesh, 64)
                .unwrap()
                .to_image()
                .as_raw()
        );
    }

    #[test]
    fn tile_coverage_keeps_material_set_filtering_separate() {
        let mut mesh = Mesh::demo_cube();
        mesh.materials.push("Second".into());
        mesh.triangle_materials[0] = 1;
        let tile = UvTileCoord { u: 0, v: 0 };
        let full = UvCoverage::generate_for_tile(&mesh, 64, tile).unwrap();
        let filtered =
            UvCoverage::generate_for_tile_and_set(&mesh, 64, &[None, Some(2)], 1, tile).unwrap();
        assert!(filtered.covered_texels() < full.covered_texels());
        assert!(UvCoverage::generate_for_tile(&mesh, 64, UvTileCoord { u: 10, v: 0 }).is_err());
    }

    #[test]
    fn extreme_finite_uvs_keep_covered_texel_math_finite_and_consistent() {
        let mut mesh = small_triangle();
        mesh.vertices[0].uv = [1.0e20, 1.0e20];
        mesh.vertices[1].uv = [-1.0e20, -1.0e20];
        mesh.vertices[2].uv = [1.0e20, 1.0e20 + 1.0e14];
        let coverage = UvCoverage::generate(&mesh, 64).unwrap();
        // This large triangle covers every texel. The prior f32 edge math
        // overflowed to NaN and incorrectly reported zero covered texels.
        assert_eq!(coverage.covered_texels(), 64 * 64);
    }
}

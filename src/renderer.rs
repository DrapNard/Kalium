//! Deterministic reference viewport. The desktop UI runs on wgpu; this
//! software rasterizer provides predictable UV hit-testing for the first alpha.
//! A native wgpu mesh/compute path is a planned milestone, not implemented here.
use crate::document::{Channel, Document};
use crate::mesh::Mesh;
use glam::{Vec2, Vec3};
use image::RgbaImage;
use std::collections::HashMap;

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
    fn resolve(&self, material_slot: usize, bindings: &[Option<u32>]) -> &Material {
        let id = bindings
            .get(material_slot)
            .copied()
            .flatten()
            .unwrap_or(self.active_set_id);
        self.sets.get(&id).unwrap_or_else(|| self.active())
    }
}

#[derive(Clone, Copy)]
struct ScreenVertex {
    xy: Vec2,
    z: f32,
    uv: Vec2,
    n: Vec3,
    p: Vec3,
}

#[derive(Clone, Copy)]
struct ClipVertex {
    p: Vec3,
    n: Vec3,
    uv: Vec2,
    depth: f32,
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

/// One direct directional source and an environment-free Lambertian fill.
/// `ambient_irradiance` is a constant fill approximation, not image-based lighting.
#[derive(Clone, Copy, Debug)]
struct PbrLighting {
    direction_to_light: Vec3,
    direct_radiance: Vec3,
    ambient_irradiance: Vec3,
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
/// masking, plus a constant Lambertian ambient fill. All colors and directions
/// are linear-light; normals and directions point toward the viewer/light.
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
    let ndot_v = n.dot(v).clamp(0.0, 1.0);
    let ndot_l = n.dot(l).clamp(0.0, 1.0);

    let f0 = Vec3::splat(0.04).lerp(base_color, metallic);
    let ambient = base_color * (1.0 - metallic) * (Vec3::ONE - f0) * ambient_irradiance * (ao / PI);

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

    finite_color(ambient + direct + emissive)
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
                depth: NEAR_PLANE,
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
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}
impl Viewport {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width * height * 4],
            uv: vec![None; width * height],
            material_hits: vec![None; width * height],
            yaw: 0.42,
            pitch: 0.25,
            distance: 4.2,
        }
    }
    /// Resize the software framebuffer without resetting the orbit camera.
    /// Keeps hit testing aligned with the displayed 3D editor after docking.
    pub fn resize(&mut self, width: usize, height: usize) {
        let width = width.clamp(128, 1100);
        let height = height.clamp(128, 900);
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.rgba.resize(width * height * 4, 0);
        self.uv.resize(width * height, None);
        self.uv.fill(None);
        self.material_hits.resize(width * height, None);
        self.material_hits.fill(None);
    }
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw += dx * 0.012;
        self.pitch = (self.pitch - dy * 0.012).clamp(-1.45, 1.45);
    }
    pub fn zoom(&mut self, delta: f32) {
        self.distance = (self.distance * (-delta * 0.001).exp()).clamp(1.6, 15.0);
    }
    pub fn pick(&self, x: usize, y: usize) -> Option<[f32; 2]> {
        if x >= self.width || y >= self.height {
            None
        } else {
            self.uv[y * self.width + x]
        }
    }
    pub fn pick_material_slot(&self, x: usize, y: usize) -> Option<usize> {
        if x >= self.width || y >= self.height {
            None
        } else {
            self.material_hits[y * self.width + x]
        }
    }

    pub fn render(&mut self, mesh: &Mesh, mat: &Material, solo: Option<Channel>) {
        self.render_internal(mesh, mat, None, solo);
    }

    pub fn render_bound(
        &mut self,
        mesh: &Mesh,
        library: &MaterialLibrary,
        bindings: &[Option<u32>],
        solo: Option<Channel>,
    ) {
        self.render_internal(mesh, library.active(), Some((library, bindings)), solo);
    }

    fn render_internal(
        &mut self,
        mesh: &Mesh,
        default_mat: &Material,
        bound: Option<(&MaterialLibrary, &[Option<u32>])>,
        solo: Option<Channel>,
    ) {
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
        let mut depths = vec![f32::INFINITY; size];
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
        let project = |p: Vec3, n: Vec3, uv: Vec2| -> ScreenVertex {
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
                p,
            }
        };
        let lighting = PbrLighting {
            direction_to_light: Vec3::new(0.3, 0.83, 0.55).normalize(),
            direct_radiance: Vec3::splat(1.8),
            ambient_irradiance: Vec3::splat(0.35),
        };
        for (triangle_index, face) in mesh.indices.chunks_exact(3).enumerate() {
            let material_slot = mesh.triangle_material(triangle_index);
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
                    depth: (p - eye).dot(forward),
                }
            }));
            if clipped.len() < 3 {
                continue;
            }
            for triangle_index in 1..clipped.len() - 1 {
                let a = project(clipped[0].p, clipped[0].n, clipped[0].uv);
                let b = project(
                    clipped[triangle_index].p,
                    clipped[triangle_index].n,
                    clipped[triangle_index].uv,
                );
                let c = project(
                    clipped[triangle_index + 1].p,
                    clipped[triangle_index + 1].n,
                    clipped[triangle_index + 1].uv,
                );
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
                let (tangent, tangent_handedness) = if det.abs() > 0.000001 {
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
                        if z >= depths[idx] {
                            continue;
                        }
                        depths[idx] = z;
                        let uv = (a.uv * ia + b.uv * ib + c.uv * ic) * z;
                        let n = (a.n * ia + b.n * ib + c.n * ic).normalize_or_zero();
                        let p = (a.p * ia + b.p * ib + c.p * ic) * z;
                        let normal = if solo.is_none() {
                            let tex = mat.get(Channel::Normal, uv);
                            let nt = Vec3::new(
                                (tex[0] as f32 / 255.0) * 2.0 - 1.0,
                                (tex[1] as f32 / 255.0) * 2.0 - 1.0,
                                (tex[2] as f32 / 255.0) * 2.0 - 1.0,
                            );
                            let t = (tangent - n * tangent.dot(n)).normalize_or_zero();
                            let bitangent = n.cross(t) * tangent_handedness;
                            (t * nt.x + bitangent * nt.y + n * nt.z).normalize_or_zero()
                        } else {
                            n
                        };
                        let diffuse = mat.get(solo.unwrap_or(Channel::BaseColor), uv);
                        let rgb = if solo.is_some() {
                            [diffuse[0], diffuse[1], diffuse[2]]
                        } else {
                            let shaded = shade_pbr(
                                material_surface(mat, uv),
                                normal,
                                (eye - p).normalize_or_zero(),
                                lighting,
                            );
                            linear_rgb_to_srgb(shaded)
                        };
                        let off = idx * 4;
                        self.rgba[off..off + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                        self.uv[idx] = Some(uv.to_array());
                        self.material_hits[idx] = Some(material_slot);
                    }
                }
            }
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
                    extensions: Default::default(),
                })
                .collect(),
            indices: vec![0, 1, 2],
            materials: vec!["first".into(), "picked".into()],
            triangle_materials: vec![material_slot],
            extensions: Default::default(),
        }
    }

    fn ray_intersection_uv(view: &Viewport, mesh: &Mesh, x: usize, y: usize) -> Option<[f32; 2]> {
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
        if beta < 0.0 || gamma < 0.0 || beta + gamma > 1.0 || distance <= 0.0 {
            return None;
        }
        let alpha = 1.0 - beta - gamma;
        let uv = Vec2::from_array(a.uv) * alpha
            + Vec2::from_array(b.uv) * beta
            + Vec2::from_array(c.uv) * gamma;
        Some(uv.to_array())
    }

    #[test]
    fn resizing_preserves_camera_and_pick_buffer_dimensions() {
        let mut view = Viewport::new(200, 200);
        view.orbit(10.0, -5.0);
        let yaw = view.yaw;
        let pitch = view.pitch;
        view.resize(480, 320);
        assert_eq!((view.width, view.height), (480, 320));
        assert_eq!(view.yaw, yaw);
        assert_eq!(view.pitch, pitch);
        assert!(view.pick(481, 200).is_none());
        assert_eq!(view.rgba.len(), 480 * 320 * 4);
        assert_eq!(view.material_hits.len(), 480 * 320);
    }
    #[test]
    fn renderer_uses_bound_set_even_when_it_is_not_active() {
        let mut doc = Document::new(64);
        doc.mesh.materials.push("Leather".into());
        doc.mesh.triangle_materials[0] = 1;
        doc.material_bindings.push(None);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(32, 32, image::Rgba([10, 20, 30, 255]));
        let leather = doc.add_texture_set("Leather").unwrap();
        doc.selected_layer_mut()
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
    fn reference_pbr_render_is_deterministic_and_solo_maps_stay_unmodified() {
        let doc = Document::new(64);
        let material = Material::from_document(&doc);
        let mut first = Viewport::new(200, 200);
        first.render(&doc.mesh, &material, None);
        let center = (100 * first.width + 100) * 4;
        let reference_rgb = [126_u8, 122, 134];
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

    #[test]
    fn edited_material_map_changes_the_next_reference_frame() {
        let mut doc = Document::new(64);
        let mesh = doc.mesh.clone();
        let mut viewport = Viewport::new(200, 200);

        let initial_material = Material::from_document(&doc);
        viewport.render(&mesh, &initial_material, None);
        let initial_frame = viewport.rgba.clone();

        let base_color = doc.selected_layer_mut().pixels_mut(Channel::BaseColor, 64);
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
        let uv = rendered
            .pick(100, 100)
            .expect("triangle covers sample pixel");
        assert!((uv[0] - 0.35).abs() < 0.002, "unexpected U: {}", uv[0]);
        assert!((uv[1] - 0.50).abs() < 0.002, "unexpected V: {}", uv[1]);
        assert_eq!(rendered.pick_material_slot(100, 100), Some(1));
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

        let uv = rendered
            .pick(100, 100)
            .expect("overlapping triangle covers sample");
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
    }

    #[test]
    fn mirrored_uv_normal_map_preserves_world_space_bitangent() {
        let view = Viewport::new(200, 200);
        let mut doc = Document::new(64);
        for pixel in doc
            .selected_layer_mut()
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

        let (index, uv) = rendered
            .uv
            .iter()
            .enumerate()
            .find_map(|(index, uv)| uv.map(|uv| (index, uv)))
            .expect("clipped triangle remains pickable");
        let (x, y) = (index % rendered.width, index / rendered.width);
        let expected = ray_intersection_uv(&rendered, &mesh, x, y)
            .expect("the picked sample ray intersects the source triangle");
        assert!(
            (uv[0] - expected[0]).abs() < 0.002,
            "picked {uv:?}, expected {expected:?}"
        );
        assert!(
            (uv[1] - expected[1]).abs() < 0.002,
            "picked {uv:?}, expected {expected:?}"
        );
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

        let (index, uv) = rendered
            .uv
            .iter()
            .enumerate()
            .find_map(|(index, uv)| uv.map(|uv| (index, uv)))
            .expect("clipped triangle remains pickable");
        let (x, y) = (index % rendered.width, index / rendered.width);
        let expected = ray_intersection_uv(&rendered, &mesh, x, y)
            .expect("the picked sample ray intersects the source triangle");
        assert!(
            (uv[0] - expected[0]).abs() < 0.002,
            "picked {uv:?}, expected {expected:?}"
        );
        assert!(
            (uv[1] - expected[1]).abs() < 0.002,
            "picked {uv:?}, expected {expected:?}"
        );
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

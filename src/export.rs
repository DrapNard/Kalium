use crate::document::{Channel, Document};
use crate::uv::UvCoverage;
use anyhow::{anyhow, Context, Result};
use image::{Rgba, RgbaImage};
use std::path::Path;

/// Export settings for per-texture-set maps.
///
/// `padding_radius` is measured in texels. Padding copies the nearest texel
/// covered by that texture set's UVs into uncovered texels, up to this radius.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    pub padding_radius: u32,
    pub normal_y: NormalYConvention,
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
    for channel in Channel::ALL {
        let mut image = composite(channel)?;
        if channel == Channel::Normal && normal_y == NormalYConvention::DirectX {
            invert_normal_green(&mut image);
        }
        let path = dir.join(format!("Kalium_{}.png", channel.filename()));
        image
            .save(&path)
            .with_context(|| format!("Writing {}", path.display()))?;
    }
    let mut base = composite(Channel::BaseColor)?;
    let opacity = composite(Channel::Opacity)?;
    for (base, opa) in base.pixels_mut().zip(opacity.pixels()) {
        base.0[3] = opa.0[0];
    }
    base.save(dir.join("Kalium_BaseColor_Alpha.png"))?;
    let metal = composite(Channel::Metallic)?;
    let rough = composite(Channel::Roughness)?;
    let ao = composite(Channel::Occlusion)?;
    let mut orm = RgbaImage::new(size, size);
    let mut unity = RgbaImage::new(size, size);
    for y in 0..size {
        for x in 0..size {
            let occ = ao.get_pixel(x, y).0[0];
            let r = rough.get_pixel(x, y).0[0];
            let m = metal.get_pixel(x, y).0[0];
            orm.put_pixel(x, y, Rgba([occ, r, m, 255]));
            unity.put_pixel(x, y, Rgba([m, 0, 0, 255 - r]));
        }
    }
    orm.save(dir.join("Kalium_ORM_Unreal_Godot.png"))?;
    unity.save(dir.join("Kalium_MetallicSmoothness_Unity.png"))?;
    Ok(11)
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
    if options.padding_radius > MAX_PADDING_RADIUS {
        return Err(anyhow!(
            "UV padding radius must be at most {MAX_PADDING_RADIUS} texels"
        ));
    }
    if options.padding_radius == 0 {
        return write_maps(doc.size, dir, options.normal_y, |channel| {
            Ok(doc.composite(channel))
        });
    }
    let coverage = UvCoverage::generate_for_set(
        &doc.mesh,
        doc.size,
        &doc.material_bindings,
        doc.active_set_id,
    )?;
    let nearest = nearest_covered_texels(&coverage);
    write_padded_maps(
        doc.size,
        dir,
        options.padding_radius,
        options.normal_y,
        &coverage,
        &nearest,
        |channel| Ok(doc.composite(channel)),
    )
}

/// Exports each set into a distinct numbered folder; titles are sanitized for
/// filesystem safety and IDs prevent collisions between identical names.
pub fn export_all_texture_sets(doc: &Document, dir: &Path) -> Result<usize> {
    doc.validate()?;
    let mut count = 0;
    for (id, name, size, _) in doc.texture_set_list() {
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
        let directory = dir.join(format!("{id:03}_{safe}"));
        count += write_maps(size, &directory, NormalYConvention::OpenGl, |channel| {
            doc.composite_for_set(id, channel)
                .ok_or_else(|| anyhow!("Unknown texture set {id}"))
        })?;
    }
    Ok(count)
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
    if options.padding_radius > MAX_PADDING_RADIUS {
        return Err(anyhow!(
            "UV padding radius must be at most {MAX_PADDING_RADIUS} texels"
        ));
    }
    doc.validate()?;
    let mut count = 0;
    for (id, name, size, _) in doc.texture_set_list() {
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
        let directory = dir.join(format!("{id:03}_{safe}"));
        let composite = |channel| {
            doc.composite_for_set(id, channel)
                .ok_or_else(|| anyhow!("Unknown texture set {id}"))
        };
        count += if options.padding_radius == 0 {
            write_maps(size, &directory, options.normal_y, composite)?
        } else {
            let coverage =
                UvCoverage::generate_for_set(&doc.mesh, size, &doc.material_bindings, id)?;
            let nearest = nearest_covered_texels(&coverage);
            write_padded_maps(
                size,
                &directory,
                options.padding_radius,
                options.normal_y,
                &coverage,
                &nearest,
                composite,
            )?
        };
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mesh::{Mesh, Vertex},
        uv::UvCoverage,
    };

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
            extensions: Default::default(),
        }
    }

    fn fill_base_color(doc: &mut Document, color: [u8; 4]) {
        let size = doc.size;
        doc.selected_layer_mut()
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
}

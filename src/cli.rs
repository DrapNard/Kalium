//! Headless commands usable by artists and CI without starting a GUI.
use crate::{
    bake, brush_preset,
    document::{Channel, MAX_DECODED_BYTES},
    export, project,
    renderer::{Material, Viewport},
    spp,
    uv::UvCoverage,
};
#[cfg(feature = "gpu-experimental")]
use crate::{gpu_brush, paint::Brush};
use anyhow::{anyhow, bail, Result};
use std::path::Path;

pub fn run(args: &[String]) -> Result<()> {
    let usage = "Kalium by DrapNard — headless commands\n\
      Usage:\n\
        kalium --help | --version\n\
        kalium inspect <project.kalium>\n\
        kalium validate <project.kalium>\n\
        kalium export <project.kalium> <output-directory>\n\
        kalium export-all <project.kalium> <output-directory>\n\
        kalium preview <project.kalium> <preview.png>\n\
        kalium uv-mask <project.kalium> <coverage.png>\n\
        kalium bake-mesh-maps <project.kalium> <output-directory> [texture-set-id]\n\
        kalium bake-ao <project.kalium> <output.png> [texture-set-id] [samples] [max-distance] [seed]\n\
        kalium spp-inspect <project.spp>\n\
        kalium brush-inspect <preset.kbrush>\n\
        kalium gpu-stamp <in.png> <out.png> <u> <v> <radius>  [requires gpu-experimental feature]\n\
      Run without arguments to launch the graphical application.";
    let arg = |i: usize| -> Result<&str> {
        args.get(i)
            .map(String::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing argument {i}.\n{usage}"))
    };
    match arg(0)? {
        "--help" | "-h" | "help" => println!("{usage}"),
        "--version" | "-V" | "version" => println!(
            "Kalium {} • DrapNard • AGPL-3.0-only",
            env!("CARGO_PKG_VERSION")
        ),
        "inspect" => {
            exact_count(args, 2, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            println!("Kalium project: {}", arg(1)?);
            println!(
                "Texture sets: {} | Active: {} (id {})",
                doc.texture_set_count(),
                doc.active_set_name,
                doc.active_set_id
            );
            println!(
                "Texture: {}×{} | Mesh: {}",
                doc.size, doc.size, doc.mesh.name
            );
            for (id, name, size, active) in doc.texture_set_list() {
                println!(
                    "  Texture Set {id}: {name} ({size}×{size}){}",
                    if active { " [active]" } else { "" }
                );
            }
            println!(
                "Vertices: {} | Triangles: {}",
                doc.mesh.vertices.len(),
                doc.mesh.indices.len() / 3
            );
            println!(
                "Layers: {} | Selected: {}",
                doc.layers.len(),
                doc.selected + 1
            );
            for (i, layer) in doc.layers.iter().enumerate() {
                let channels: Vec<_> = Channel::ALL
                    .into_iter()
                    .filter(|ch| layer.pixels.contains_key(ch))
                    .map(Channel::filename)
                    .collect();
                println!(
                    "  {}. {} (id {}, visible {}, opacity {:.2}, {:?}) [{}]",
                    i + 1,
                    layer.name,
                    layer.id,
                    layer.visible,
                    layer.opacity,
                    layer.blend,
                    channels.join(", ")
                );
            }
        }
        "validate" => {
            exact_count(args, 2, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            doc.validate()?;
            println!("VALID: {} (native Kalium format v1–v6)", arg(1)?);
        }
        "export" => {
            exact_count(args, 3, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            let n = export::export_textures(&doc, Path::new(arg(2)?))?;
            println!("Exported {n} maps to {}", arg(2)?);
        }
        "export-all" => {
            exact_count(args, 3, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            let n = export::export_all_texture_sets(&doc, Path::new(arg(2)?))?;
            println!(
                "Exported {n} maps from {} texture sets to {}",
                doc.texture_set_count(),
                arg(2)?
            );
        }
        "preview" => {
            exact_count(args, 3, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            let material = Material::from_document(&doc);
            let mut view = Viewport::new(600, 600);
            view.render(&doc.mesh, &material, None);
            let image = image::RgbaImage::from_raw(600, 600, view.rgba)
                .ok_or_else(|| anyhow!("Viewport buffer size mismatch"))?;
            image.save(arg(2)?)?;
            println!("Rendered 600×600 reference preview: {}", arg(2)?);
        }
        "uv-mask" => {
            exact_count(args, 3, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            let coverage = UvCoverage::generate(&doc.mesh, doc.size)?;
            coverage.to_image().save(arg(2)?)?;
            println!(
                "Wrote UV coverage map: {} ({} / {} texels covered)",
                arg(2)?,
                coverage.covered_texels(),
                (doc.size as usize) * (doc.size as usize)
            );
        }
        "bake-mesh-maps" => {
            if args.len() != 3 && args.len() != 4 {
                bail!(
                    "Expected a project, output directory, and optional Texture Set ID.\n{usage}"
                );
            }
            let doc = project::load(Path::new(arg(1)?))?;
            let set_id = args
                .get(3)
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(doc.active_set_id);
            let Some((_, _, size, _)) = doc
                .texture_set_list()
                .into_iter()
                .find(|(id, _, _, _)| *id == set_id)
            else {
                bail!("Unknown Texture Set ID {set_id}");
            };
            let remaining_budget = MAX_DECODED_BYTES.saturating_sub(doc.decoded_bytes());
            let maps = bake::bake_mesh_maps_budgeted(
                &doc.mesh,
                size,
                &doc.material_bindings,
                set_id,
                remaining_budget,
                &std::sync::atomic::AtomicBool::new(false),
            )?;
            let output_dir = Path::new(arg(2)?);
            std::fs::create_dir_all(output_dir)?;
            maps.world_normal_image()
                .save(output_dir.join("Kalium_WorldNormal.png"))?;
            maps.normalized_position_image()
                .save(output_dir.join("Kalium_Position.png"))?;
            image::GrayImage::from_fn(size, size, |x, y| {
                image::Luma([if maps.is_valid(x, y) { 255 } else { 0 }])
            })
            .save(output_dir.join("Kalium_BakeCoverage.png"))?;
            let metadata = serde_json::json!({
                "format": "Kalium Mesh Maps",
                "version": 1,
                "texture_set_id": set_id,
                "resolution": size,
                "texture_color_space": "linear data (import as Non-Color)",
                "normal_space": "world",
                "normal_encoding": "signed direction mapped from [-1,1] to [0,255] RGB",
                "position_encoding": "mesh bounds normalized to [0,1] RGB",
                "position_min": maps.position_min,
                "position_max": maps.position_max,
                "position_zero_extent_axis_encoding": 0.5,
                "uv_overlap_policy": "first covering triangle in mesh index order",
                "covered_texels": maps.covered_texels(),
                "limitations": ["same-mesh attributes only", "no high-to-low projection", "no dilation"]
            });
            std::fs::write(
                output_dir.join("Kalium_MeshMap_Metadata.json"),
                serde_json::to_vec_pretty(&metadata)?,
            )?;
            println!(
                "Baked world-normal and position maps for Texture Set {set_id} ({} × {}, {} covered texels) to {}",
                size,
                size,
                maps.covered_texels(),
                output_dir.display()
            );
        }
        "bake-ao" => {
            if args.len() < 3 || args.len() > 7 {
                bail!("Expected a project, output PNG, and optional Texture Set ID, samples, max distance, and seed.\n{usage}");
            }
            let doc = project::load(Path::new(arg(1)?))?;
            let (set_id, samples, max_distance, seed) = parse_ao_args(args, doc.active_set_id)?;
            let Some((_, _, size, _)) = doc
                .texture_set_list()
                .into_iter()
                .find(|(id, _, _, _)| *id == set_id)
            else {
                bail!("Unknown Texture Set ID {set_id}");
            };
            let remaining_budget = MAX_DECODED_BYTES.saturating_sub(doc.decoded_bytes());
            // Account for the encoded RGBA output image as well as the primitive's
            // own maps and acceleration structure in the shared decoded-memory cap.
            let output_bytes = u64::from(size)
                .checked_mul(u64::from(size))
                .and_then(|texels| texels.checked_mul(4))
                .ok_or_else(|| anyhow!("Ambient occlusion output size overflow"))?;
            let working_budget = remaining_budget.saturating_sub(output_bytes);
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let ao = bake::bake_ambient_occlusion(
                &doc.mesh,
                bake::AmbientOcclusionBakeOptions {
                    size,
                    bindings: &doc.material_bindings,
                    active_set_id: set_id,
                    sample_count: samples,
                    max_ray_distance: max_distance,
                    seed,
                    working_budget_bytes: working_budget,
                },
                &cancel,
            )?;
            let mut image = image::RgbaImage::new(size, size);
            for (index, pixel) in image.pixels_mut().enumerate() {
                if ao.valid[index] == 0 {
                    *pixel = image::Rgba([0, 0, 0, 0]);
                } else {
                    // Grayscale convention: white is open, black is occluded.
                    let open = (1.0 - ao.occlusion[index]).clamp(0.0, 1.0);
                    let value = (open * 255.0).round() as u8;
                    *pixel = image::Rgba([value, value, value, 255]);
                }
            }
            image.save(arg(2)?)?;
            println!(
                "Baked ambient occlusion for Texture Set {set_id} ({} × {}, {} samples, {} covered texels) to {}",
                size,
                size,
                samples,
                ao.valid.iter().filter(|&&valid| valid != 0).count(),
                arg(2)?
            );
        }
        "spp-inspect" => {
            exact_count(args, 2, usage)?;
            println!("{}", spp::inspect(Path::new(arg(1)?))?);
        }
        "brush-inspect" => {
            exact_count(args, 2, usage)?;
            let (name, brush) = brush_preset::load(Path::new(arg(1)?))?;
            println!(
                "Brush: {name} | radius={}px | opacity={:.2} | hardness={:.2} | spacing={:.2}",
                brush.radius, brush.opacity, brush.hardness, brush.spacing
            );
        }
        #[cfg(feature = "gpu-experimental")]
        "gpu-stamp" => {
            exact_count(args, 6, usage)?;
            let path = Path::new(arg(1)?);
            let (width, height) = image::image_dimensions(path)?;
            if width < 1 || height < 1 || width > 4096 || height > 4096 {
                bail!("GPU brush input dimensions must be 1..=4096");
            }
            let mut img = image::open(path)?.to_rgba8();
            let u: f32 = arg(3)?.parse()?;
            let v: f32 = arg(4)?.parse()?;
            let radius: u32 = arg(5)?.parse()?;
            let mut brush = Brush::default();
            brush.radius = radius;
            gpu_brush::stamp(&mut img, &brush, [u, v])?;
            img.save(arg(2)?)?;
            println!("Experimental wgpu brush output saved: {}", arg(2)?);
        }
        cmd => bail!("Unknown command: {cmd}\n{usage}"),
    }
    Ok(())
}

fn parse_ao_args(args: &[String], active_set_id: u32) -> Result<(u32, u32, f32, u64)> {
    if args.len() < 3 || args.len() > 7 {
        bail!("bake-ao expects 2 to 6 arguments after the command");
    }
    let set_id: u32 = args
        .get(3)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(active_set_id);
    let samples: u32 = args
        .get(4)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(16);
    if !(1..=4096).contains(&samples) {
        bail!("Ambient occlusion samples must be between 1 and 4096");
    }
    let max_distance: f32 = args
        .get(5)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1.0);
    if !max_distance.is_finite() || max_distance <= 0.0 {
        bail!("Ambient occlusion max distance must be finite and greater than zero");
    }
    let seed: u64 = args
        .get(6)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(0);
    Ok((set_id, samples, max_distance, seed))
}

fn exact_count(args: &[String], n: usize, usage: &str) -> Result<()> {
    if args.len() != n {
        bail!(
            "Expected {} command arguments, got {}.\n{usage}",
            n - 1,
            args.len() - 1
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bad_arguments_return_errors() {
        assert!(run(&["export".into(), "foo.kalium".into()]).is_err());
        assert!(run(&["unknown".into()]).is_err());
    }
    #[test]
    fn help_is_ok() {
        assert!(run(&["--help".into()]).is_ok());
    }

    #[test]
    fn bake_ao_defaults_and_explicit_options_parse() {
        assert_eq!(
            parse_ao_args(&["bake-ao".into(), "p".into(), "o".into()], 7).unwrap(),
            (7, 16, 1.0, 0)
        );
        assert_eq!(
            parse_ao_args(
                &[
                    "bake-ao".into(),
                    "p".into(),
                    "o".into(),
                    "3".into(),
                    "64".into(),
                    "2.5".into(),
                    "99".into()
                ],
                7
            )
            .unwrap(),
            (3, 64, 2.5, 99)
        );
    }

    #[test]
    fn bake_ao_rejects_bad_ranges_and_values() {
        for samples in ["0", "4097", "nope"] {
            assert!(parse_ao_args(
                &[
                    "bake-ao".into(),
                    "p".into(),
                    "o".into(),
                    "1".into(),
                    samples.into()
                ],
                1
            )
            .is_err());
        }
        for distance in ["0", "-1", "NaN", "inf", "bad"] {
            assert!(parse_ao_args(
                &[
                    "bake-ao".into(),
                    "p".into(),
                    "o".into(),
                    "1".into(),
                    "8".into(),
                    distance.into()
                ],
                1
            )
            .is_err());
        }
        assert!(parse_ao_args(
            &[
                "bake-ao".into(),
                "p".into(),
                "o".into(),
                "1".into(),
                "8".into(),
                "1".into(),
                "-1".into()
            ],
            1
        )
        .is_err());
        assert!(parse_ao_args(&["bake-ao".into(), "p".into(), "o".into(), "x".into()], 1).is_err());
        assert!(parse_ao_args(
            &[
                "bake-ao".into(),
                "p".into(),
                "o".into(),
                "1".into(),
                "2".into(),
                "1".into(),
                "0".into(),
                "extra".into()
            ],
            1
        )
        .is_err());
    }

    #[test]
    fn bake_ao_cli_writes_grayscale_png_with_transparent_uv_gaps() {
        let project_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Kalium-Demo.kalium");
        let output_path =
            std::env::temp_dir().join(format!("kalium-cli-ao-{}.png", uuid::Uuid::new_v4()));
        let baseline_digest = project::file_digest(&project_path).unwrap();
        let args = vec![
            "bake-ao".to_owned(),
            project_path.display().to_string(),
            output_path.display().to_string(),
            "1".to_owned(),
            "1".to_owned(),
            "1.0".to_owned(),
            "42".to_owned(),
        ];

        run(&args).unwrap();

        let image = image::open(&output_path).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (512, 512));
        let mut covered = 0usize;
        for pixel in image.pixels() {
            if pixel[3] == 0 {
                assert_eq!(pixel.0, [0, 0, 0, 0]);
            } else {
                covered += 1;
                assert_eq!(pixel[0], pixel[1]);
                assert_eq!(pixel[1], pixel[2]);
                assert_eq!(pixel[3], 255);
            }
        }
        assert_eq!(covered, 262_144);
        assert_eq!(
            project::file_digest(&project_path).unwrap(),
            baseline_digest
        );
        std::fs::remove_file(output_path).unwrap();
    }
}

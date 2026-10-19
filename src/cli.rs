//! Headless commands usable by artists and CI without starting a GUI.
use crate::{
    bake, brush_preset,
    document::{Channel, MAX_DECODED_BYTES},
    export,
    layer_tree::LayerNode,
    mesh::Mesh,
    project,
    renderer::{Material, Viewport},
    spp,
    uv::UvCoverage,
};
#[cfg(feature = "gpu-experimental")]
use crate::{gpu_brush, paint::Brush};
use anyhow::{anyhow, bail, Result};
use glam::Vec3;
use serde_json::Value;
use std::{io::Read, path::Path};

pub fn run(args: &[String]) -> Result<()> {
    let usage = "Kalium by DrapNard — headless commands\n\
      Usage:\n\
        kalium --help | --version\n\
        kalium inspect <project.kalium>\n\
        kalium validate <project.kalium>\n\
        kalium export <project.kalium> <output-directory>\n\
        kalium export-all <project.kalium> <output-directory>\n\
        kalium export-godot <project.kalium> <output-directory>\n\
        kalium export-all-godot <project.kalium> <output-directory>\n\
        kalium export-all-unity <project.kalium> <output-directory>\n\
        kalium export-gltf <project.kalium> <output-directory>\n\
        kalium export-all-gltf <project.kalium> <output-directory>\n\
        kalium export-painter <project.kalium> <output-directory>\n\
        kalium export-all-painter <project.kalium> <output-directory>\n\
        kalium preview <project.kalium> <preview.png>\n\
        kalium uv-mask <project.kalium> <coverage.png>\n\
        kalium bake-mesh-maps <project.kalium> <output-directory> [texture-set-id]\n\
        kalium bake-material-id <project.kalium> <output.png> [texture-set-id]\n\
        kalium bake-ao <project.kalium> <output.png> [texture-set-id] [samples] [max-distance] [seed]\n\
        kalium bake-curvature <project.kalium> <output.png> [texture-set-id] [samples] [radius] [seed]\n\
        kalium bake-thickness <project.kalium> <output.png> [texture-set-id] [samples] [max-distance] [seed]\n\
        kalium bake-high-to-low <low-project.kalium> <high-mesh.obj|gltf|glb> <output-directory> [texture-set-id] [max-distance] [high-transform.json]\n\
          With a transform, it maps source high-mesh coordinates to low-project coordinates. Otherwise imported high-mesh coordinates are used directly.\n\
          The JSON transform is 16 column-major affine numbers; transformed coordinates are capped at 1,000,000 units.\n\
          Max distance uses low-project units after any transform; no cage is used.\n\
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
                "Layer tree: {} nodes | Selected node: {}",
                doc.layer_tree.all_node_ids().len(),
                doc.layer_tree.selected_node_id
            );
            for node in &doc.layer_tree.roots {
                print_layer_node(node, doc.layer_tree.selected_node_id, 1);
            }
        }
        "validate" => {
            exact_count(args, 2, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            doc.validate()?;
            println!("VALID: {} (native Kalium format v1–v9)", arg(1)?);
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
        "export-godot" | "export-all-godot" | "export-all-unity" | "export-gltf"
        | "export-all-gltf" | "export-painter" | "export-all-painter" => {
            exact_count(args, 3, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            let options = export::ExportOptions {
                profile: match arg(0)? {
                    "export-all-unity" => export::EngineExportProfile::UnityMetallic,
                    "export-gltf" | "export-all-gltf" => {
                        export::EngineExportProfile::GltfMetallicRoughness
                    }
                    "export-painter" | "export-all-painter" => {
                        export::EngineExportProfile::SubstancePainterPython
                    }
                    _ => export::EngineExportProfile::Godot4Orm,
                },
                ..export::ExportOptions::default()
            };
            let all_sets = !matches!(arg(0)?, "export-godot" | "export-gltf" | "export-painter");
            let count = if all_sets {
                export::export_all_texture_sets_with_options(&doc, Path::new(arg(2)?), options)?
            } else {
                export::export_textures_with_options(&doc, Path::new(arg(2)?), options)?
            };
            println!(
                "Exported {count} PNG maps with {} profile to {}",
                options.profile.label(),
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
        "bake-material-id" => {
            if args.len() != 3 && args.len() != 4 {
                bail!("Expected a project, output PNG, and optional Texture Set ID.\n{usage}");
            }
            let project_path = Path::new(arg(1)?);
            let output_path = Path::new(arg(2)?);
            if output_path
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("png")
            {
                bail!("Material-ID output path must end in .png");
            }
            let doc = project::load(project_path)?;
            let set_id = args
                .get(3)
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(doc.active_set_id);
            if set_id == 0 {
                bail!("Texture Set ID must be nonzero");
            }
            let Some((_, set_name, size, _)) = doc
                .texture_set_list()
                .into_iter()
                .find(|(id, _, _, _)| *id == set_id)
            else {
                bail!("Unknown Texture Set ID {set_id}");
            };
            let output_bytes = u64::from(size)
                .checked_mul(u64::from(size))
                .and_then(|texels| texels.checked_mul(4))
                .ok_or_else(|| anyhow!("Material-ID output size overflow"))?;
            let working_budget = MAX_DECODED_BYTES.saturating_sub(doc.decoded_bytes());
            if output_bytes > working_budget {
                bail!("Material-ID image exceeds the remaining decoded texture budget");
            }
            let image = bake::bake_material_id(
                &doc.mesh,
                size,
                &doc.material_bindings,
                set_id,
                working_budget,
                &std::sync::atomic::AtomicBool::new(false),
            )?;

            let metadata = bake::material_id_metadata(
                &doc.mesh,
                size,
                set_id,
                &set_name,
                set_id == doc.active_set_id,
            )?;
            let sidecar_path = bake::write_material_id_outputs(&image, &metadata, output_path)?;
            println!(
                "Baked material IDs for Texture Set {set_id} ({} × {}) to {} with metadata {}",
                size,
                size,
                output_path.display(),
                sidecar_path.display()
            );
        }
        "bake-high-to-low" => {
            if args.len() < 4 || args.len() > 7 {
                bail!("Expected a low project, high mesh, output directory, and optional Texture Set ID, max distance, and high-mesh transform.\n{usage}");
            }
            let project_path = Path::new(arg(1)?);
            let high_mesh_path = Path::new(arg(2)?);
            let output_dir = Path::new(arg(3)?);
            let doc = project::load(project_path)?;
            let set_id = args
                .get(4)
                .map(|value| value.parse::<u32>())
                .transpose()?
                .unwrap_or(doc.active_set_id);
            if set_id == 0 {
                bail!("Texture Set ID must be nonzero");
            }
            let max_distance = args
                .get(5)
                .map(|value| value.parse::<f32>())
                .transpose()?
                .unwrap_or(1.0);
            if !max_distance.is_finite() || max_distance <= 0.0 {
                bail!("Maximum bake ray distance must be finite and greater than zero");
            }
            let Some((_, set_name, size, _)) = doc
                .texture_set_list()
                .into_iter()
                .find(|(id, _, _, _)| *id == set_id)
            else {
                bail!("Unknown Texture Set ID {set_id}");
            };
            let high_source_bytes = std::fs::metadata(high_mesh_path)?.len();
            let (high_mesh, applied_transform) = if let Some(transform_path) = args.get(6) {
                let mut mesh = Mesh::from_file_preserving_coordinates(high_mesh_path)?;
                let transform = read_high_transform(Path::new(transform_path))?;
                apply_high_mesh_transform(&mut mesh, transform)?;
                (mesh, Some(transform))
            } else {
                (Mesh::from_file(high_mesh_path)?, None)
            };
            let high_mesh_bytes = estimate_mesh_transient_bytes(&high_mesh, high_source_bytes)?;
            let texels = u64::from(size)
                .checked_mul(u64::from(size))
                .ok_or_else(|| anyhow!("High-to-low output dimensions overflow"))?;
            let coverage_bytes = texels;
            let working_budget =
                high_to_low_working_budget(doc.decoded_bytes(), high_mesh_bytes, coverage_bytes)?;
            let maps = bake::bake_high_to_low_world_normals(
                &doc.mesh,
                &high_mesh,
                bake::HighToLowBakeOptions {
                    size,
                    bindings: &doc.material_bindings,
                    active_set_id: set_id,
                    max_ray_distance: max_distance,
                    working_budget_bytes: working_budget,
                },
                &std::sync::atomic::AtomicBool::new(false),
            )?;
            let metadata = serde_json::json!({
                "format": "Kalium High-to-Low World Normal Projection",
                "version": 1,
                "texture_set": {"id": set_id, "name": set_name},
                "dimensions": {"width": size, "height": size},
                "normal_space": "world",
                "normal_encoding": "signed direction mapped from [-1,1] to [0,255] RGB; uncovered texels transparent",
                "coordinate_space_assumption": if applied_transform.is_some() { "the supplied affine transform maps source OBJ or glTF-node-transformed high-mesh coordinates to low-project coordinates" } else { "the high mesh is projected using Kalium viewport-normalized imported coordinates directly as low-project coordinates" },
                "high_mesh_transform_column_major": applied_transform.map(HighMeshTransform::to_column_major_array),
                "high_mesh_transform_matrix_convention": "16 column-major values; affine matrix acting on column-vector positions",
                "max_ray_distance_units": "low project mesh coordinate units after applying the high-mesh transform, when present",
                "max_ray_distance": max_distance,
                "covered_texels": maps.covered_texels(),
                "limitations": ["no cage workflow", "no dilation", "outputs are standalone maps and are not persisted as project assets"]
            });
            bake::write_high_to_low_outputs(&maps, &metadata, output_dir)?;
            println!("Projected high-mesh world normals onto Texture Set {set_id} ({} × {}, {} covered texels) to {}", size, size, maps.covered_texels(), output_dir.display());
        }
        "bake-curvature" | "bake-thickness" => {
            let doc = project::load(Path::new(arg(1)?))?;
            let (set_id, samples, distance, seed) = parse_ao_args(args, doc.active_set_id)?;
            let size = doc
                .texture_set_list()
                .into_iter()
                .find(|(id, _, _, _)| *id == set_id)
                .map(|(_, _, size, _)| size)
                .ok_or_else(|| anyhow!("Unknown Texture Set ID {set_id}"))?;
            let options = bake::SurfaceMapBakeOptions {
                size,
                bindings: &doc.material_bindings,
                active_set_id: set_id,
                max_ray_distance: distance,
                curvature_radius: distance,
                sample_count: samples,
                seed,
                working_budget_bytes: MAX_DECODED_BYTES.saturating_sub(doc.decoded_bytes()),
            };
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let image = if arg(0)? == "bake-curvature" {
                bake::bake_curvature(&doc.mesh, options, &cancel)?
            } else {
                bake::bake_thickness(&doc.mesh, options, &cancel)?
            };
            image.save(arg(2)?)?;
            println!(
                "Baked {} for Texture Set {set_id} ({} × {}) to {}",
                arg(0)?.trim_start_matches("bake-"),
                size,
                size,
                arg(2)?
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

fn print_layer_node(node: &LayerNode, selected_node_id: u32, depth: usize) {
    let indent = "  ".repeat(depth);
    let selected = if node.id() == selected_node_id {
        " [selected]"
    } else {
        ""
    };
    match node {
        LayerNode::Paint(layer) => {
            let channels: Vec<_> = Channel::ALL
                .into_iter()
                .filter(|channel| layer.pixels.contains_key(channel))
                .map(Channel::filename)
                .collect();
            println!(
                "{indent}Paint: {} (id {}, visible {}, opacity {:.2}, {:?}) [{}]{selected}",
                layer.name,
                layer.id,
                layer.visible,
                layer.opacity,
                layer.blend,
                channels.join(", ")
            );
        }
        LayerNode::Fill(fill) => {
            let channels: Vec<_> = Channel::ALL
                .into_iter()
                .filter(|channel| fill.values.contains_key(channel))
                .map(Channel::filename)
                .collect();
            println!(
                "{indent}Fill: {} (id {}, visible {}, opacity {:.2}, {:?}) [{}]{selected}",
                fill.properties.name,
                fill.properties.id,
                fill.properties.visible,
                fill.properties.opacity,
                fill.properties.blend,
                channels.join(", ")
            );
        }
        LayerNode::Group(group) => {
            println!(
                "{indent}Group: {} (id {}, visible {}, opacity {:.2}, {:?}){selected}",
                group.properties.name,
                group.properties.id,
                group.properties.visible,
                group.properties.opacity,
                group.properties.blend
            );
            for child in &group.children {
                print_layer_node(child, selected_node_id, depth + 1);
            }
        }
    }
}

fn estimate_mesh_transient_bytes(mesh: &Mesh, source_bytes: u64) -> Result<u64> {
    let vector_bytes = (mesh.vertices.capacity() as u64)
        .checked_mul(std::mem::size_of::<crate::mesh::Vertex>() as u64)
        .and_then(|bytes| bytes.checked_add((mesh.indices.capacity() as u64).checked_mul(4)?))
        .and_then(|bytes| {
            bytes.checked_add((mesh.triangle_materials.capacity() as u64).checked_mul(4)?)
        })
        .and_then(|bytes| {
            bytes.checked_add(
                (mesh.materials.capacity() as u64)
                    .checked_mul(std::mem::size_of::<String>() as u64)?,
            )
        })
        .and_then(|bytes| bytes.checked_add(mesh.name.capacity() as u64))
        .and_then(|bytes| {
            mesh.materials
                .iter()
                .try_fold(bytes, |sum, name| sum.checked_add(name.capacity() as u64))
        })
        .ok_or_else(|| anyhow!("High-mesh memory estimate overflow"))?;
    // Reserve for retained source data and the assembled mesh. Unknown JSON
    // fields and parser scratch are covered conservatively by the source size.
    source_bytes
        .checked_add(vector_bytes)
        .ok_or_else(|| anyhow!("High-mesh memory estimate overflow"))
}

const HIGH_TRANSFORM_MAX_BYTES: u64 = 4096;
const HIGH_TRANSFORM_MAX_COORDINATE: f32 = 1_000_000.0;

#[derive(Clone, Copy)]
struct HighMeshTransform {
    columns: [Vec3; 3],
    translation: Vec3,
}

impl HighMeshTransform {
    fn linear(self) -> glam::Mat3 {
        glam::Mat3::from_cols(self.columns[0], self.columns[1], self.columns[2])
    }

    fn to_column_major_array(self) -> [f32; 16] {
        [
            self.columns[0].x,
            self.columns[0].y,
            self.columns[0].z,
            0.0,
            self.columns[1].x,
            self.columns[1].y,
            self.columns[1].z,
            0.0,
            self.columns[2].x,
            self.columns[2].y,
            self.columns[2].z,
            0.0,
            self.translation.x,
            self.translation.y,
            self.translation.z,
            1.0,
        ]
    }
}

fn read_high_transform(path: &Path) -> Result<HighMeshTransform> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(HIGH_TRANSFORM_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > HIGH_TRANSFORM_MAX_BYTES {
        bail!("High-mesh transform exceeds the 4096-byte limit");
    }
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!("Invalid high-mesh transform JSON: {error}"))?;
    let values = json
        .as_array()
        .filter(|values| values.len() == 16)
        .ok_or_else(|| anyhow!("High-mesh transform must be a JSON array of exactly 16 numbers"))?;
    let mut matrix = [0.0f32; 16];
    for (index, value) in values.iter().enumerate() {
        let number = value
            .as_f64()
            .ok_or_else(|| anyhow!("High-mesh transform entry {index} must be a number"))?;
        let narrowed = number as f32;
        if !number.is_finite() || !narrowed.is_finite() {
            bail!("High-mesh transform entry {index} must be finite and representable as f32");
        }
        matrix[index] = narrowed;
    }
    // glTF stores matrices column-major. The final row must describe an
    // affine transform; tolerate only float serialization noise here.
    const AFFINE_EPSILON: f32 = 1.0e-6;
    if matrix[3].abs() > AFFINE_EPSILON
        || matrix[7].abs() > AFFINE_EPSILON
        || matrix[11].abs() > AFFINE_EPSILON
        || (matrix[15] - 1.0).abs() > AFFINE_EPSILON
    {
        bail!("High-mesh transform must be affine in glTF column-major convention");
    }
    let transform = HighMeshTransform {
        columns: [
            Vec3::new(matrix[0], matrix[1], matrix[2]),
            Vec3::new(matrix[4], matrix[5], matrix[6]),
            Vec3::new(matrix[8], matrix[9], matrix[10]),
        ],
        translation: Vec3::new(matrix[12], matrix[13], matrix[14]),
    };
    let determinant = transform.linear().determinant();
    if !determinant.is_finite() || determinant == 0.0 {
        bail!("High-mesh transform linear part must be invertible");
    }
    let normal_matrix = transform.linear().inverse().transpose();
    if !normal_matrix.is_finite() {
        bail!("High-mesh transform inverse is non-finite");
    }
    Ok(transform)
}

fn apply_high_mesh_transform(mesh: &mut Mesh, transform: HighMeshTransform) -> Result<()> {
    let linear = transform.linear();
    let determinant = linear.determinant();
    if !determinant.is_finite() || determinant == 0.0 {
        bail!("High-mesh transform linear part must be invertible");
    }
    let normal_matrix = linear.inverse().transpose();
    if !normal_matrix.is_finite() {
        bail!("High-mesh transform inverse is non-finite");
    }
    for vertex in &mut mesh.vertices {
        let position = linear * Vec3::from_array(vertex.position) + transform.translation;
        let source_normal = Vec3::from_array(vertex.normal);
        let normal = (normal_matrix * source_normal).normalize_or_zero();
        if !position.is_finite() || !normal.is_finite() || normal.length_squared() == 0.0 {
            bail!("High-mesh transform produced a non-finite or degenerate position/normal");
        }
        if position.abs().max_element() > HIGH_TRANSFORM_MAX_COORDINATE {
            bail!("High-mesh transform produced a coordinate beyond the {HIGH_TRANSFORM_MAX_COORDINATE} safety limit");
        }
        vertex.position = position.to_array();
        vertex.normal = normal.to_array();
        if let Some(tangent) = vertex.tangent {
            let source_tangent = Vec3::new(tangent[0], tangent[1], tangent[2]);
            let transformed_tangent = linear * source_tangent;
            let orthogonal = transformed_tangent - normal * transformed_tangent.dot(normal);
            let tangent_direction = orthogonal.normalize_or_zero();
            if !tangent_direction.is_finite()
                || tangent_direction.length_squared() == 0.0
                || !tangent[3].is_finite()
            {
                bail!("High-mesh transform produced a non-finite or degenerate tangent");
            }
            vertex.tangent = Some([
                tangent_direction.x,
                tangent_direction.y,
                tangent_direction.z,
                if determinant < 0.0 {
                    -tangent[3]
                } else {
                    tangent[3]
                },
            ]);
        }
    }
    if determinant < 0.0 {
        for triangle in mesh.indices.chunks_exact_mut(3) {
            triangle.swap(1, 2);
        }
    }
    // Invertibility mathematically preserves nonzero triangle area. Check the
    // f32 result too, since extreme scales can collapse geometry numerically.
    for triangle in mesh.indices.chunks_exact(3) {
        let a = Vec3::from_array(mesh.vertices[triangle[0] as usize].position);
        let b = Vec3::from_array(mesh.vertices[triangle[1] as usize].position);
        let c = Vec3::from_array(mesh.vertices[triangle[2] as usize].position);
        let area_vector = (b - a).cross(c - a);
        let area_squared = area_vector.length_squared();
        if !area_vector.is_finite() || !area_squared.is_finite() || area_squared == 0.0 {
            bail!("High-mesh transform collapsed or invalidated a triangle");
        }
    }
    mesh.validate()?;
    Ok(())
}

fn high_to_low_working_budget(
    project_bytes: u64,
    high_mesh_bytes: u64,
    coverage_bytes: u64,
) -> Result<u64> {
    MAX_DECODED_BYTES
        .checked_sub(project_bytes)
        .and_then(|remaining| remaining.checked_sub(high_mesh_bytes))
        .and_then(|remaining| remaining.checked_sub(coverage_bytes))
        .ok_or_else(|| {
            anyhow!("High-to-low inputs exceed the remaining decoded texture memory budget")
        })
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

    fn make_transform_file(path: &Path, values: [f32; 16]) {
        std::fs::write(path, serde_json::to_vec(&values).unwrap()).unwrap();
    }

    fn test_high_mesh() -> Mesh {
        let path = std::env::temp_dir().join(format!(
            "kalium-transform-high-{}.obj",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            "v 0 0 1\nv 1 0 1\nv 0 1 1\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let mesh = Mesh::from_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        mesh
    }

    fn mat4_values(columns: [[f32; 4]; 4]) -> [f32; 16] {
        let mut result = [0.0; 16];
        for (column_index, column) in columns.into_iter().enumerate() {
            result[column_index * 4..column_index * 4 + 4].copy_from_slice(&column);
        }
        result
    }
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
    fn bake_high_to_low_cli_projects_maps_and_preserves_source_project() {
        let suffix = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("kalium-high-to-low-{suffix}"));
        std::fs::create_dir_all(&root).unwrap();
        let project_path = root.join("low.kalium");
        let high_path = root.join("high.obj");
        let output_dir = root.join("out");
        project::save(&crate::document::Document::new(64), &project_path).unwrap();
        std::fs::write(
            &high_path,
            "o High\nv -1 -1 1.1\nv 1 -1 1.1\nv -1 1 1.1\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let original_project = std::fs::read(&project_path).unwrap();
        let doc = project::load(&project_path).unwrap();
        let high = Mesh::from_file(&high_path).unwrap();
        let source_bytes = std::fs::metadata(&high_path).unwrap().len();
        let high_bytes = estimate_mesh_transient_bytes(&high, source_bytes).unwrap();
        let texels = u64::from(doc.size) * u64::from(doc.size);
        let expected = bake::bake_high_to_low_world_normals(
            &doc.mesh,
            &high,
            bake::HighToLowBakeOptions {
                size: doc.size,
                bindings: &doc.material_bindings,
                active_set_id: doc.active_set_id,
                max_ray_distance: 1.0,
                working_budget_bytes: high_to_low_working_budget(
                    doc.decoded_bytes(),
                    high_bytes,
                    texels,
                )
                .unwrap(),
            },
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(expected.covered_texels() > 0);

        run(&[
            "bake-high-to-low".into(),
            project_path.display().to_string(),
            high_path.display().to_string(),
            output_dir.display().to_string(),
        ])
        .unwrap();
        let normal = image::open(output_dir.join("Kalium_HighToLow_WorldNormal.png"))
            .unwrap()
            .to_rgba8();
        let coverage = image::open(output_dir.join("Kalium_HighToLow_Coverage.png"))
            .unwrap()
            .to_luma8();
        assert_eq!(normal, expected.world_normal_image());
        assert_eq!(coverage.dimensions(), (doc.size, doc.size));
        assert_eq!(
            coverage.pixels().filter(|pixel| pixel.0[0] != 0).count(),
            expected.covered_texels()
        );
        let metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(output_dir.join("Kalium_HighToLow_Metadata.json")).unwrap(),
        )
        .unwrap();
        assert!(metadata["coordinate_space_assumption"]
            .as_str()
            .unwrap()
            .contains("viewport-normalized imported coordinates directly"));
        assert_eq!(metadata["covered_texels"], expected.covered_texels());
        assert_eq!(std::fs::read(&project_path).unwrap(), original_project);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bake_high_to_low_rejects_invalid_distance_and_memory_budget() {
        let suffix = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("kalium-high-to-low-invalid-{suffix}"));
        std::fs::create_dir_all(&root).unwrap();
        let project_path = root.join("low.kalium");
        let high_path = root.join("high.obj");
        project::save(&crate::document::Document::new(64), &project_path).unwrap();
        std::fs::write(
            &high_path,
            "v 0 0 1\nv 1 0 1\nv 0 1 1\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        for value in ["0", "-1", "NaN", "inf", "nope"] {
            let error = run(&[
                "bake-high-to-low".into(),
                project_path.display().to_string(),
                high_path.display().to_string(),
                root.join("out").display().to_string(),
                "1".into(),
                value.into(),
            ]);
            assert!(error.is_err(), "distance {value} should fail");
        }
        assert!(high_to_low_working_budget(1, MAX_DECODED_BYTES, 0).is_err());
        assert!(high_to_low_working_budget(MAX_DECODED_BYTES, 1, 0).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn high_transform_identity_has_cli_bake_parity_and_preserves_project() {
        let root = std::env::temp_dir().join(format!(
            "kalium-high-transform-identity-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let project_path = root.join("low.kalium");
        let high_path = root.join("high.obj");
        let transform_path = root.join("identity.json");
        let plain_output = root.join("plain");
        let transformed_output = root.join("transformed");
        project::save(&crate::document::Document::new(64), &project_path).unwrap();
        std::fs::write(
            &high_path,
            // This source bounds to the same coordinates as the legacy
            // importer normalization, so identity must preserve CLI output.
            "v -1 -1 0\nv 1 -1 0\nv -1 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let identity = mat4_values([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]);
        make_transform_file(&transform_path, identity);
        let original_project = std::fs::read(&project_path).unwrap();
        run(&[
            "bake-high-to-low".into(),
            project_path.display().to_string(),
            high_path.display().to_string(),
            plain_output.display().to_string(),
        ])
        .unwrap();
        run(&[
            "bake-high-to-low".into(),
            project_path.display().to_string(),
            high_path.display().to_string(),
            transformed_output.display().to_string(),
            "1".into(),
            "1".into(),
            transform_path.display().to_string(),
        ])
        .unwrap();
        for filename in [
            "Kalium_HighToLow_WorldNormal.png",
            "Kalium_HighToLow_Coverage.png",
        ] {
            assert_eq!(
                std::fs::read(plain_output.join(filename)).unwrap(),
                std::fs::read(transformed_output.join(filename)).unwrap()
            );
        }
        assert_eq!(std::fs::read(&project_path).unwrap(), original_project);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn high_transform_maps_source_coordinates_into_low_project_space() {
        let root = std::env::temp_dir().join(format!(
            "kalium-high-transform-alignment-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let project_path = root.join("low.kalium");
        let high_path = root.join("high.obj");
        let transform_path = root.join("transform.json");
        let output_dir = root.join("out");
        project::save(&crate::document::Document::new(64), &project_path).unwrap();
        // Source-space plane is well away from the low project's normalized
        // surface. The explicit translation places it at z=1.1 in project space.
        std::fs::write(
            &high_path,
            "v -1 -1 10\nv 1 -1 10\nv -1 1 10\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let transform_values = mat4_values([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, -8.9, 1.0],
        ]);
        make_transform_file(&transform_path, transform_values);
        let original_project = std::fs::read(&project_path).unwrap();
        let doc = project::load(&project_path).unwrap();
        let mut high = Mesh::from_file_preserving_coordinates(&high_path).unwrap();
        apply_high_mesh_transform(&mut high, read_high_transform(&transform_path).unwrap())
            .unwrap();
        assert!(high
            .vertices
            .iter()
            .all(|vertex| (vertex.position[2] - 1.1).abs() < 1e-5));
        let high_bytes =
            estimate_mesh_transient_bytes(&high, std::fs::metadata(&high_path).unwrap().len())
                .unwrap();
        let texels = u64::from(doc.size) * u64::from(doc.size);
        let expected = bake::bake_high_to_low_world_normals(
            &doc.mesh,
            &high,
            bake::HighToLowBakeOptions {
                size: doc.size,
                bindings: &doc.material_bindings,
                active_set_id: doc.active_set_id,
                max_ray_distance: 1.0,
                working_budget_bytes: high_to_low_working_budget(
                    doc.decoded_bytes(),
                    high_bytes,
                    texels,
                )
                .unwrap(),
            },
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        run(&[
            "bake-high-to-low".into(),
            project_path.display().to_string(),
            high_path.display().to_string(),
            output_dir.display().to_string(),
            "1".into(),
            "1".into(),
            transform_path.display().to_string(),
        ])
        .unwrap();
        assert!(expected.covered_texels() > 0);
        let normal = image::open(output_dir.join("Kalium_HighToLow_WorldNormal.png"))
            .unwrap()
            .to_rgba8();
        assert_eq!(normal, expected.world_normal_image());
        let metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(output_dir.join("Kalium_HighToLow_Metadata.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["high_mesh_transform_column_major"],
            serde_json::to_value(transform_values).unwrap()
        );
        assert_eq!(std::fs::read(&project_path).unwrap(), original_project);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn high_transform_applies_translation_rotation_and_nonuniform_scale() {
        let transform_dir = std::env::temp_dir().join(format!(
            "kalium-high-transform-effects-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&transform_dir).unwrap();
        let transform_path = transform_dir.join("matrix.json");
        let original = test_high_mesh();
        let mut translated = original.clone();
        make_transform_file(
            &transform_path,
            mat4_values([
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [2.0, 3.0, 4.0, 1.0],
            ]),
        );
        apply_high_mesh_transform(
            &mut translated,
            read_high_transform(&transform_path).unwrap(),
        )
        .unwrap();
        let source_position = Vec3::from_array(original.vertices[0].position);
        assert_eq!(
            translated.vertices[0].position,
            (source_position + Vec3::new(2.0, 3.0, 4.0)).to_array()
        );

        let mut rotated_scaled = original.clone();
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        for vertex in &mut rotated_scaled.vertices {
            vertex.normal = [diagonal, 0.0, diagonal];
            vertex.tangent = Some([diagonal, 0.0, -diagonal, 1.0]);
        }
        // 90-degree Z rotation followed by nonuniform scale.
        make_transform_file(
            &transform_path,
            mat4_values([
                [0.0, 2.0, 0.0, 0.0],
                [-3.0, 0.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ]),
        );
        apply_high_mesh_transform(
            &mut rotated_scaled,
            read_high_transform(&transform_path).unwrap(),
        )
        .unwrap();
        let source_position = Vec3::from_array(original.vertices[1].position);
        assert_eq!(
            rotated_scaled.vertices[1].position,
            Vec3::new(
                -3.0 * source_position.y,
                2.0 * source_position.x,
                source_position.z
            )
            .to_array()
        );
        assert_ne!(
            rotated_scaled.vertices[1].position,
            original.vertices[1].position
        );
        let transformed_normal = Vec3::from_array(rotated_scaled.vertices[0].normal);
        assert!((transformed_normal - Vec3::new(0.0, 1.0, 2.0).normalize()).length() < 1.0e-5);
        let transformed_tangent = rotated_scaled.vertices[0].tangent.unwrap();
        assert!(transformed_tangent[..3]
            .iter()
            .all(|component| component.is_finite()));
        std::fs::remove_dir_all(transform_dir).unwrap();
    }

    #[test]
    fn reflected_high_transform_reverses_winding_and_keeps_normals_and_tangents_valid() {
        let mut mesh = test_high_mesh();
        for vertex in &mut mesh.vertices {
            vertex.tangent = Some([1.0, 0.0, 0.0, 1.0]);
        }
        let source_indices = mesh.indices.clone();
        apply_high_mesh_transform(
            &mut mesh,
            HighMeshTransform {
                columns: [-Vec3::X, Vec3::Y, Vec3::Z],
                translation: Vec3::ZERO,
            },
        )
        .unwrap();
        assert_eq!(
            mesh.indices,
            [source_indices[0], source_indices[2], source_indices[1]]
        );
        let [a, b, c] = [
            Vec3::from_array(mesh.vertices[mesh.indices[0] as usize].position),
            Vec3::from_array(mesh.vertices[mesh.indices[1] as usize].position),
            Vec3::from_array(mesh.vertices[mesh.indices[2] as usize].position),
        ];
        let geometric_normal = (b - a).cross(c - a).normalize();
        for vertex in &mesh.vertices {
            let normal = Vec3::from_array(vertex.normal);
            let tangent = vertex.tangent.unwrap();
            let direction = Vec3::new(tangent[0], tangent[1], tangent[2]);
            assert!(normal.is_finite() && (normal.length() - 1.0).abs() < 1.0e-5);
            assert!(direction.is_finite() && (direction.length() - 1.0).abs() < 1.0e-5);
            assert!(normal.dot(direction).abs() < 1.0e-5);
            assert_eq!(tangent[3], -1.0);
            assert!(normal.dot(geometric_normal) > 0.99);
        }
    }

    #[test]
    fn high_transform_json_is_strict_bounded_and_invertible() {
        let root = std::env::temp_dir().join(format!(
            "kalium-high-transform-invalid-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("transform.json");
        let identity = mat4_values([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]);
        make_transform_file(&path, identity);
        assert!(read_high_transform(&path).is_ok());
        for invalid in [
            "{}".to_owned(),
            "[1, 2, 3]".to_owned(),
            "[\"1\"]".to_owned(),
            "[1e100,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1]".to_owned(),
            "[1,0,0,1,0,1,0,0,0,0,1,0,0,0,0,1]".to_owned(),
            "[1,0,0,0,0,0,0,0,0,0,1,0,0,0,0,1]".to_owned(),
        ] {
            std::fs::write(&path, &invalid).unwrap();
            assert!(read_high_transform(&path).is_err(), "accepted {invalid}");
        }
        std::fs::write(&path, vec![b' '; HIGH_TRANSFORM_MAX_BYTES as usize + 1]).unwrap();
        assert!(read_high_transform(&path).is_err());

        let mut mesh = test_high_mesh();
        let extreme_translation = HighMeshTransform {
            columns: [Vec3::X, Vec3::Y, Vec3::Z],
            translation: Vec3::splat(HIGH_TRANSFORM_MAX_COORDINATE + 1.0),
        };
        assert!(apply_high_mesh_transform(&mut mesh, extreme_translation)
            .unwrap_err()
            .to_string()
            .contains("safety limit"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bake_material_id_rejects_invalid_arguments_and_texture_set_ranges() {
        let project_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Kalium-Demo.kalium");
        let output_path = std::env::temp_dir().join(format!(
            "kalium-cli-material-id-{}.png",
            uuid::Uuid::new_v4()
        ));
        let base = vec![
            "bake-material-id".to_owned(),
            project_path.display().to_string(),
            output_path.display().to_string(),
        ];
        assert!(run(&base[..2]).is_err());
        assert!(run(&[base[0].clone(), base[1].clone(), "bad.jpg".into()]).is_err());
        for set_id in ["0", "4294967296", "not-a-number", "999999"] {
            let args = [base.clone(), vec![set_id.to_owned()]].concat();
            assert!(run(&args).is_err(), "Texture Set ID {set_id} should fail");
        }
        let args = [base, vec!["1".into(), "extra".into()]].concat();
        assert!(run(&args).is_err());
    }

    #[test]
    fn bake_material_id_cli_writes_png_and_encoding_sidecar_without_changing_project() {
        let project_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/Kalium-Demo.kalium");
        let suffix = uuid::Uuid::new_v4();
        let output_path = std::env::temp_dir().join(format!("kalium-cli-material-{suffix}.png"));
        let sidecar_path = std::path::PathBuf::from(format!("{}.json", output_path.display()));
        let original_project = std::fs::read(&project_path).unwrap();
        let doc = project::load(&project_path).unwrap();
        let expected = bake::bake_material_id(
            &doc.mesh,
            doc.size,
            &doc.material_bindings,
            doc.active_set_id,
            MAX_DECODED_BYTES.saturating_sub(doc.decoded_bytes()),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        let args = vec![
            "bake-material-id".to_owned(),
            project_path.display().to_string(),
            output_path.display().to_string(),
        ];

        run(&args).unwrap();

        let image = image::open(&output_path).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (doc.size, doc.size));
        assert_eq!(image, expected);
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar_path).unwrap()).unwrap();
        assert_eq!(metadata["format"], "Kalium Material ID");
        assert_eq!(metadata["encoding"]["slot_index_bits"], 24);
        assert_eq!(metadata["encoding"]["byte_order"], "little-endian");
        assert_eq!(metadata["encoding"]["alpha"]["covered"], 255);
        assert_eq!(metadata["encoding"]["alpha"]["uncovered"], 0);
        assert_eq!(metadata["dimensions"]["width"], doc.size);
        assert_eq!(metadata["dimensions"]["height"], doc.size);
        assert_eq!(metadata["texture_set"]["id"], doc.active_set_id);
        assert_eq!(metadata["texture_set"]["name"], doc.active_set_name);
        assert_eq!(
            metadata["material_slots"].as_array().unwrap().len(),
            doc.mesh.material_count()
        );
        for slot in 0..doc.mesh.material_count() {
            assert_eq!(metadata["material_slots"][slot]["slot_index"], slot);
            assert_eq!(
                metadata["material_slots"][slot]["name"],
                doc.mesh.material_name(slot)
            );
        }
        assert_eq!(std::fs::read(&project_path).unwrap(), original_project);
        std::fs::remove_file(output_path).unwrap();
        std::fs::remove_file(sidecar_path).unwrap();
    }

    #[test]
    fn integrated_surface_bake_commands_preserve_project_and_match_primitives() {
        let root =
            std::env::temp_dir().join(format!("kalium-combined-bakes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let doc = crate::document::Document::new(64);
        let project_path = root.join("source.kalium");
        project::save(&doc, &project_path).unwrap();
        let original = std::fs::read(&project_path).unwrap();
        for command in ["bake-curvature", "bake-thickness"] {
            let output = root.join(format!("{command}.png"));
            let args = vec![
                command.into(),
                project_path.display().to_string(),
                output.display().to_string(),
                doc.active_set_id.to_string(),
                "2".into(),
                "0.2".into(),
                "42".into(),
            ];
            run(&args).unwrap();
            let options = bake::SurfaceMapBakeOptions {
                size: 64,
                bindings: &doc.material_bindings,
                active_set_id: doc.active_set_id,
                sample_count: 2,
                seed: 42,
                max_ray_distance: 0.2,
                curvature_radius: 0.2,
                working_budget_bytes: MAX_DECODED_BYTES,
            };
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let expected = if command == "bake-curvature" {
                bake::bake_curvature(&doc.mesh, options, &cancel).unwrap()
            } else {
                bake::bake_thickness(&doc.mesh, options, &cancel).unwrap()
            };
            assert_eq!(image::open(output).unwrap().to_rgba8(), expected);
            assert_eq!(std::fs::read(&project_path).unwrap(), original);
            let mut invalid = args.clone();
            invalid[3] = u32::MAX.to_string();
            assert!(run(&invalid).is_err());
            invalid = args;
            invalid[5] = "NaN".into();
            assert!(run(&invalid).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
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

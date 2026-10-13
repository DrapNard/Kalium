//! Headless commands usable by artists and CI without starting a GUI.
use crate::{brush_preset, document::Channel, export, project, renderer::{Material,Viewport}, spp, uv::UvCoverage};
#[cfg(feature="gpu-experimental")]
use crate::{gpu_brush,paint::Brush};
use anyhow::{anyhow,bail, Result};
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
        kalium spp-inspect <project.spp>\n\
        kalium brush-inspect <preset.kbrush>\n\
        kalium gpu-stamp <in.png> <out.png> <u> <v> <radius>  [requires gpu-experimental feature]\n\
      Run without arguments to launch the graphical application.";
    let arg = |i: usize| -> Result<&str> {
        args.get(i).map(String::as_str).ok_or_else(|| anyhow::anyhow!("Missing argument {i}.\n{usage}"))
    };
    match arg(0)? {
        "--help" | "-h" | "help" => println!("{usage}"),
        "--version" | "-V" | "version" => println!("Kalium {} • DrapNard • AGPL-3.0-only", env!("CARGO_PKG_VERSION")),
        "inspect" => {
            exact_count(args, 2, usage)?;
            let doc = project::load(Path::new(arg(1)?))?;
            println!("Kalium project: {}", arg(1)?);
            println!("Texture sets: {} | Active: {} (id {})",doc.texture_set_count(),doc.active_set_name,doc.active_set_id);
            println!("Texture: {}×{} | Mesh: {}", doc.size, doc.size, doc.mesh.name);
            for (id,name,size,active) in doc.texture_set_list() {
                println!("  Texture Set {id}: {name} ({size}×{size}){}",if active {" [active]"} else {""});
            }
            println!("Vertices: {} | Triangles: {}", doc.mesh.vertices.len(), doc.mesh.indices.len()/3);
            println!("Layers: {} | Selected: {}", doc.layers.len(), doc.selected + 1);
            for (i, layer) in doc.layers.iter().enumerate() {
                let channels: Vec<_> = Channel::ALL.into_iter()
                    .filter(|ch| layer.pixels.contains_key(ch))
                    .map(Channel::filename).collect();
                println!("  {}. {} (id {}, visible {}, opacity {:.2}, {:?}) [{}]", i+1,
                    layer.name, layer.id, layer.visible, layer.opacity, layer.blend, channels.join(", "));
            }
        }
        "validate" => {
            exact_count(args, 2, usage)?;
            let doc=project::load(Path::new(arg(1)?))?;
            doc.validate()?;
            println!("VALID: {} (native Kalium format v1/v2/v3/v4)", arg(1)?);
        }
        "export" => {
            exact_count(args, 3, usage)?;
            let doc=project::load(Path::new(arg(1)?))?;
            let n=export::export_textures(&doc,Path::new(arg(2)?))?;
            println!("Exported {n} maps to {}",arg(2)?);
        }
        "export-all" => {
            exact_count(args,3,usage)?;
            let doc=project::load(Path::new(arg(1)?))?;
            let n=export::export_all_texture_sets(&doc,Path::new(arg(2)?))?;
            println!("Exported {n} maps from {} texture sets to {}",doc.texture_set_count(),arg(2)?);
        }
        "preview" => {
            exact_count(args, 3, usage)?;
            let doc=project::load(Path::new(arg(1)?))?;
            let material=Material::from_document(&doc);
            let mut view=Viewport::new(600,600);
            view.render(&doc.mesh,&material,None);
            let image=image::RgbaImage::from_raw(600,600,view.rgba)
                .ok_or_else(||anyhow!("Viewport buffer size mismatch"))?;
            image.save(arg(2)?)?;
            println!("Rendered 600×600 reference preview: {}",arg(2)?);
        }
        "uv-mask" => {
            exact_count(args, 3, usage)?;
            let doc=project::load(Path::new(arg(1)?))?;
            let coverage=UvCoverage::generate(&doc.mesh,doc.size)?;
            coverage.to_image().save(arg(2)?)?;
            println!("Wrote UV coverage map: {} ({} / {} texels covered)",
                arg(2)?, coverage.covered_texels(), (doc.size as usize)*(doc.size as usize));
        }
        "spp-inspect" => {
            exact_count(args, 2, usage)?;
            println!("{}",spp::inspect(Path::new(arg(1)?))?);
        }
        "brush-inspect" => {
            exact_count(args, 2, usage)?;
            let (name,brush)=brush_preset::load(Path::new(arg(1)?))?;
            println!("Brush: {name} | radius={}px | opacity={:.2} | hardness={:.2} | spacing={:.2}",
                brush.radius,brush.opacity,brush.hardness,brush.spacing);
        }
        #[cfg(feature="gpu-experimental")]
        "gpu-stamp" => {
            exact_count(args, 6, usage)?;
            let path=Path::new(arg(1)?);
            let (width,height)=image::image_dimensions(path)?;
            if width<1||height<1||width>4096||height>4096 {
                bail!("GPU brush input dimensions must be 1..=4096");
            }
            let mut img=image::open(path)?.to_rgba8();
            let u:f32=arg(3)?.parse()?;
            let v:f32=arg(4)?.parse()?;
            let radius:u32=arg(5)?.parse()?;
            let mut brush=Brush::default();
            brush.radius=radius;
            gpu_brush::stamp(&mut img,&brush,[u,v])?;
            img.save(arg(2)?)?;
            println!("Experimental wgpu brush output saved: {}",arg(2)?);
        }
        cmd => bail!("Unknown command: {cmd}\n{usage}"),
    }
    Ok(())
}

fn exact_count(args: &[String], n: usize, usage: &str) -> Result<()> {
    if args.len()!=n {bail!("Expected {} command arguments, got {}.\n{usage}", n-1, args.len()-1);}
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
    fn help_is_ok() { assert!(run(&["--help".into()]).is_ok()); }
}

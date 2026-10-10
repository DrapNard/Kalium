//! Headless commands usable by artists and CI without starting a GUI.
use crate::{brush_preset, document::Channel, export, project, renderer::{Material,Viewport}, spp};
use anyhow::{anyhow,bail, Result};
use std::path::Path;

pub fn run(args: &[String]) -> Result<()> {
    let usage = "Kalium by DrapNard — headless commands\n\
      Usage:\n\
        kalium --help | --version\n\
        kalium inspect <project.kalium>\n\
        kalium validate <project.kalium>\n\
        kalium export <project.kalium> <output-directory>\n\
        kalium preview <project.kalium> <preview.png>\n\
        kalium spp-inspect <project.spp>\n\
        kalium brush-inspect <preset.kbrush>\n\
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
            println!("Texture: {}×{} | Mesh: {}", doc.size, doc.size, doc.mesh.name);
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
            println!("VALID: {} (native Kalium format v1)", arg(1)?);
        }
        "export" => {
            exact_count(args, 3, usage)?;
            let doc=project::load(Path::new(arg(1)?))?;
            let n=export::export_textures(&doc,Path::new(arg(2)?))?;
            println!("Exported {n} maps to {}",arg(2)?);
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

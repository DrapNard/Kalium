use anyhow::{bail, Context, Result};
use glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MAX_TRIANGLES: usize = 350_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    /// Top-left origin: [0, 0] is the top-left corner of a texture.
    pub uv: [f32; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mesh {
    pub name: String,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Material slots in source mesh order. Empty means legacy single-material mesh.
    #[serde(default)]
    pub materials: Vec<String>,
    /// One material-slot index per triangle. Empty means all triangles use slot 0.
    #[serde(default)]
    pub triangle_materials: Vec<u32>,
}

impl Mesh {
    pub fn demo_cube() -> Self {
        // Six independently UV-mapped cube faces, arranged in a 3x2 atlas.
        let faces = [
            ([0.0, 0.0, 1.0], [[-1.,-1., 1.],[ 1.,-1., 1.],[ 1., 1., 1.],[-1., 1., 1.]]),
            ([0.0, 0.0,-1.0], [[ 1.,-1.,-1.],[-1.,-1.,-1.],[-1., 1.,-1.],[ 1., 1.,-1.]]),
            ([1.0, 0.0, 0.0], [[ 1.,-1., 1.],[ 1.,-1.,-1.],[ 1., 1.,-1.],[ 1., 1., 1.]]),
            ([-1., 0.0, 0.0], [[-1.,-1.,-1.],[-1.,-1., 1.],[-1., 1., 1.],[-1., 1.,-1.]]),
            ([0.0, 1.0, 0.0], [[-1., 1., 1.],[ 1., 1., 1.],[ 1., 1.,-1.],[-1., 1.,-1.]]),
            ([0.0,-1.0, 0.0], [[-1.,-1.,-1.],[ 1.,-1.,-1.],[ 1.,-1., 1.],[-1.,-1., 1.]]),
        ];
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for (face, (normal, positions)) in faces.iter().enumerate() {
            let x = (face % 3) as f32 / 3.0;
            let y = (face / 3) as f32 / 2.0;
            let uv = [[x, y + 0.5], [x + 1.0 / 3.0, y + 0.5], [x + 1.0 / 3.0, y], [x, y]];
            let base = vertices.len() as u32;
            for i in 0..4 {
                vertices.push(Vertex { position: positions[i], normal: *normal, uv: uv[i] });
            }
            indices.extend_from_slice(&[base,base+1,base+2,base,base+2,base+3]);
        }
        Self { name: "Kalium demo cube".into(), vertices, indices, materials: vec!["Default".into()], triangle_materials: vec![0;12] }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        match extension.as_str() {
            "obj" => Self::from_obj(path),
            "gltf" | "glb" => Self::from_gltf(path),
            _ => bail!("Unsupported mesh format. Use .obj, .gltf or .glb"),
        }
    }

    fn from_obj(path: &Path) -> Result<Self> {
        let (models, source_materials) = tobj::load_obj(path, &tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        }).with_context(|| format!("Opening {}", path.display()))?;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut materials:Vec<String> = source_materials.unwrap_or_default().iter()
            .enumerate().map(|(index,m)| {
                if m.name.trim().is_empty() { format!("Material {}",index+1) }
                else { m.name.clone() }
            }).collect();
        let mut triangle_materials = Vec::new();
        for model in models {
            let m = model.mesh;
            let material_index = m.material_id.filter(|i| *i < materials.len()).map(|i| i as u32).unwrap_or(u32::MAX);
            triangle_materials.extend(std::iter::repeat(material_index).take(m.indices.len() / 3));
            let offset = vertices.len() as u32;
            let vertex_count = m.positions.len() / 3;
            for i in 0..vertex_count {
                let pos = [m.positions[i*3],m.positions[i*3+1],m.positions[i*3+2]];
                let normal = if m.normals.len() >= i*3+3 {
                    [m.normals[i*3],m.normals[i*3+1],m.normals[i*3+2]]
                } else { [0.0;3] };
                let uv = if m.texcoords.len() >= i*2+2 {
                    [m.texcoords[i*2],1.0-m.texcoords[i*2+1]]
                } else { [0.0,0.0] };
                vertices.push(Vertex {position:pos, normal, uv});
            }
            indices.extend(m.indices.into_iter().map(|i| offset + i));
        }
        Self::finish_material_slots(&mut materials,&mut triangle_materials);
        let mut mesh = Self { name: path.file_name().map(|n|n.to_string_lossy().into_owned()).unwrap_or_else(||"Imported Mesh".into()), vertices, indices, materials, triangle_materials };
        mesh.finish_import()?;
        Ok(mesh)
    }

    fn from_gltf(path: &Path) -> Result<Self> {
        let (gltf, buffers, _images) = gltf::import(path)
            .with_context(|| format!("Importing glTF {}", path.display()))?;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut triangle_materials = Vec::new();
        let mut materials:Vec<String> = gltf.materials().enumerate().map(|(i, m)| {
            m.name().filter(|s| !s.trim().is_empty()).map(str::to_owned)
                .unwrap_or_else(|| format!("Material {}", i + 1))
        }).collect();
        let scenes: Vec<_> = gltf.default_scene().map(|s| vec![s]).unwrap_or_else(|| gltf.scenes().collect());
        for scene in scenes {
            for node in scene.nodes() {
                Self::read_gltf_node(node, Mat4::IDENTITY, &buffers, &mut vertices, &mut indices, &mut triangle_materials)?;
            }
        }
        Self::finish_material_slots(&mut materials,&mut triangle_materials);
        let mut mesh = Self { name: path.file_name().map(|n|n.to_string_lossy().into_owned()).unwrap_or_else(||"Imported Mesh".into()), vertices, indices, materials, triangle_materials };
        mesh.finish_import()?;
        Ok(mesh)
    }

    fn read_gltf_node(
        node: gltf::Node<'_>, parent: Mat4,
        buffers: &[gltf::buffer::Data], vertices: &mut Vec<Vertex>, indices: &mut Vec<u32>, triangle_materials: &mut Vec<u32>,
    ) -> Result<()> {
        let transform = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles { continue; }
                let reader = primitive.reader(|b| Some(&buffers[b.index()]));
                let Some(positions) = reader.read_positions() else { continue };
                let ps: Vec<_> = positions.collect();
                let ns: Vec<_> = reader.read_normals().map(|iter| iter.collect()).unwrap_or_default();
                let uv: Vec<_> = reader.read_tex_coords(0)
                    .map(|c| c.into_f32().collect()).unwrap_or_default();
                let ids: Vec<u32> = reader.read_indices().map(|i| i.into_u32().collect())
                    .unwrap_or_else(|| (0..ps.len() as u32).collect());
                let offset = vertices.len() as u32;
                let normal_transform = transform.inverse().transpose();
                for (i, pos) in ps.into_iter().enumerate() {
                    let p = transform.transform_point3(Vec3::from_array(pos));
                    let n = ns.get(i).map(|n| normal_transform.transform_vector3(Vec3::from_array(*n)).normalize_or_zero())
                        .unwrap_or(Vec3::ZERO);
                    vertices.push(Vertex {
                        position: p.to_array(), normal: n.to_array(),
                        uv: uv.get(i).copied().unwrap_or([0.0,0.0]),
                    });
                }
                let material_index = primitive.material().index().map(|i| i as u32).unwrap_or(u32::MAX);
                triangle_materials.extend(std::iter::repeat(material_index).take(ids.len() / 3));
                indices.extend(ids.into_iter().map(|i| offset+i));
            }
        }
        for child in node.children() {
            Self::read_gltf_node(child, transform, buffers, vertices, indices, triangle_materials)?;
        }
        Ok(())
    }

    /// Uses the actual source material slots, adding a Default slot only if a
    /// triangle has no material assigned. u32::MAX is an internal import sentinel.
    fn finish_material_slots(materials:&mut Vec<String>, triangle_materials:&mut [u32]) {
        if materials.is_empty() || triangle_materials.contains(&u32::MAX) {
            let index=materials.len() as u32;
            materials.push("Default".to_owned());
            for slot in triangle_materials {
                if *slot==u32::MAX { *slot=index; }
            }
        }
    }

    fn finish_import(&mut self) -> Result<()> {
        self.validate()?;
        if self.vertices.iter().any(|v| Vec3::from_array(v.normal).length_squared() < 0.001) {
            for v in &mut self.vertices { v.normal = [0.0;3]; }
            for ids in self.indices.chunks_exact(3) {
                let (a,b,c) = (ids[0] as usize,ids[1] as usize,ids[2] as usize);
                let pa=Vec3::from_array(self.vertices[a].position);
                let pb=Vec3::from_array(self.vertices[b].position);
                let pc=Vec3::from_array(self.vertices[c].position);
                let n=(pb-pa).cross(pc-pa);
                for k in [a,b,c] {
                    let v = Vec3::from_array(self.vertices[k].normal) + n;
                    self.vertices[k].normal = v.to_array();
                }
            }
            for v in &mut self.vertices {v.normal = Vec3::from_array(v.normal).normalize_or_zero().to_array();}
        }
        // Normalize to a convenient viewport scale; the source asset is NOT overwritten.
        let mut min=Vec3::splat(f32::INFINITY);
        let mut max=Vec3::splat(f32::NEG_INFINITY);
        for v in &self.vertices { let p=Vec3::from_array(v.position);min=min.min(p);max=max.max(p); }
        let center=(min+max)*0.5;
        let scale = 2.0/(max-min).max_element().max(1e-6);
        for v in &mut self.vertices { v.position = ((Vec3::from_array(v.position)-center)*scale).to_array(); }
        Ok(())
    }

    pub fn material_count(&self) -> usize { self.materials.len().max(1) }

    pub fn material_name(&self, slot: usize) -> &str {
        self.materials.get(slot).map(String::as_str).unwrap_or("Default")
    }

    pub fn triangle_material(&self, triangle: usize) -> usize {
        self.triangle_materials.get(triangle).copied().unwrap_or(0) as usize
    }

    pub fn validate(&self) -> Result<()> {
        if self.vertices.is_empty() || self.indices.is_empty() || self.indices.len()%3!=0 {
            bail!("Model is empty or not made of triangles");
        }
        if !self.triangle_materials.is_empty() && self.triangle_materials.len() != self.indices.len()/3 {
            bail!("Material slot count must match triangle count");
        }
        if self.material_count() > 256 { bail!("Mesh exceeds 256 material slots"); }
        if self.materials.iter().any(|name| name.len() > 256 || name.contains('\0')) {
            bail!("Invalid mesh material name");
        }
        if self.triangle_materials.iter().any(|slot| (*slot as usize) >= self.material_count()) {
            bail!("Triangle references an unknown material slot");
        }
        if self.indices.len()/3 > MAX_TRIANGLES {bail!("Model exceeds the {} triangle safety limit",MAX_TRIANGLES);}
        for i in &self.indices { if *i as usize >= self.vertices.len() { bail!("Invalid mesh index {i}"); } }
        for v in &self.vertices {
            if v.position.iter().chain(v.normal.iter()).chain(v.uv.iter()).any(|n| !n.is_finite()) {
                bail!("Model has invalid float coordinates");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cube_has_six_faces_and_uvs() {
        let mesh=Mesh::demo_cube();
        mesh.validate().unwrap();
        assert_eq!(mesh.indices.len(),36);
        assert!(mesh.vertices.iter().all(|v| v.uv.iter().all(|n| (0.0..=1.0).contains(n))));
    }
    #[test]
    fn obj_import_keeps_source_material_slots() {
        let dir=std::env::temp_dir().join(format!("kalium-mtl-test-{}",std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj=dir.join("demo.obj");
        std::fs::write(dir.join("demo.mtl"),"newmtl Metal\nKd 0.5 0.5 0.5\nnewmtl Leather\nKd 0.4 0.3 0.2\n").unwrap();
        std::fs::write(&obj, "mtllib demo.mtl\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nusemtl Metal\nf 1/1 2/2 3/3\nusemtl Leather\nf 1/1 3/3 4/4\n").unwrap();
        let mesh=Mesh::from_file(&obj).unwrap();
        assert_eq!(mesh.materials,vec!["Metal".to_owned(),"Leather".to_owned()]);
        assert_eq!(mesh.triangle_materials,vec![0,1]);
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn mesh_material_slots_are_validated() {
        let mut mesh = Mesh::demo_cube();
        mesh.triangle_materials[0] = 1;
        assert!(mesh.validate().is_err());
        mesh.materials.push("Leather".into());
        mesh.validate().unwrap();
        mesh.triangle_materials.pop();
        assert!(mesh.validate().is_err());
    }
    #[test]
    fn reject_bad_triangle_index() {
        let mut mesh=Mesh::demo_cube();mesh.indices[0]=999;
        assert!(mesh.validate().is_err());
    }
}

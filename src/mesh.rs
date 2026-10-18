use anyhow::{bail, Context, Result};
use glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::{
    cell::Cell,
    fs::File,
    io::{BufReader, Read},
    path::{Component, Path, PathBuf},
};

pub const MAX_TRIANGLES: usize = 350_000;
// These limits bound the in-memory mesh assembled by this module. Format
// decoders may allocate source buffers before these traversal limits apply.
const MAX_IMPORTED_VERTICES: usize = 1_050_000;
const MAX_IMPORTED_INDICES: usize = MAX_TRIANGLES * 3;
const MAX_GLTF_NODE_DEPTH: usize = 256;
const MAX_GLTF_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_GLTF_BUFFER_BYTES: usize = 128 * 1024 * 1024;
const MAX_OBJ_SOURCE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    /// Top-left origin: [0, 0] is the top-left corner of a texture.
    pub uv: [f32; 2],
    /// Unknown mesh-manifest fields retained for forward-compatible project round trips.
    #[serde(flatten)]
    pub(crate) extensions: JsonMap<String, JsonValue>,
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
    /// Unknown mesh-manifest fields retained for forward-compatible project round trips.
    #[serde(flatten)]
    pub(crate) extensions: JsonMap<String, JsonValue>,
}

impl Mesh {
    pub fn demo_cube() -> Self {
        // Six independently UV-mapped cube faces, arranged in a 3x2 atlas.
        let faces = [
            (
                [0.0, 0.0, 1.0],
                [[-1., -1., 1.], [1., -1., 1.], [1., 1., 1.], [-1., 1., 1.]],
            ),
            (
                [0.0, 0.0, -1.0],
                [
                    [1., -1., -1.],
                    [-1., -1., -1.],
                    [-1., 1., -1.],
                    [1., 1., -1.],
                ],
            ),
            (
                [1.0, 0.0, 0.0],
                [[1., -1., 1.], [1., -1., -1.], [1., 1., -1.], [1., 1., 1.]],
            ),
            (
                [-1., 0.0, 0.0],
                [
                    [-1., -1., -1.],
                    [-1., -1., 1.],
                    [-1., 1., 1.],
                    [-1., 1., -1.],
                ],
            ),
            (
                [0.0, 1.0, 0.0],
                [[-1., 1., 1.], [1., 1., 1.], [1., 1., -1.], [-1., 1., -1.]],
            ),
            (
                [0.0, -1.0, 0.0],
                [
                    [-1., -1., -1.],
                    [1., -1., -1.],
                    [1., -1., 1.],
                    [-1., -1., 1.],
                ],
            ),
        ];
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for (face, (normal, positions)) in faces.iter().enumerate() {
            let x = (face % 3) as f32 / 3.0;
            let y = (face / 3) as f32 / 2.0;
            let uv = [
                [x, y + 0.5],
                [x + 1.0 / 3.0, y + 0.5],
                [x + 1.0 / 3.0, y],
                [x, y],
            ];
            let base = vertices.len() as u32;
            for i in 0..4 {
                vertices.push(Vertex {
                    position: positions[i],
                    normal: *normal,
                    uv: uv[i],
                    extensions: JsonMap::new(),
                });
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        Self {
            name: "Kalium demo cube".into(),
            vertices,
            indices,
            materials: vec!["Default".into()],
            triangle_materials: vec![0; 12],
            extensions: JsonMap::new(),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        match extension.as_str() {
            "obj" => Self::from_obj(path),
            "gltf" | "glb" => Self::from_gltf(path),
            _ => bail!("Unsupported mesh format. Use .obj, .gltf or .glb"),
        }
    }

    fn from_obj(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
        let obj_len = file.metadata()?.len();
        if obj_len > MAX_OBJ_SOURCE_BYTES {
            bail!("OBJ source exceeds the {MAX_OBJ_SOURCE_BYTES} byte safety limit");
        }
        let base = asset_base_dir(path)?;
        let total_source_bytes = Cell::new(obj_len);
        let mut reader = BufReader::new(file.take(MAX_OBJ_SOURCE_BYTES + 1));
        let (models, source_materials) = tobj::load_obj_buf(
            &mut reader,
            &tobj::LoadOptions {
                triangulate: true,
                single_index: true,
                ..Default::default()
            },
            |material_path| {
                let resolved = base.join(material_path).canonicalize();
                let Ok(resolved) = resolved else {
                    return Err(tobj::LoadError::OpenFileFailed);
                };
                if !resolved.starts_with(&base) {
                    return Err(tobj::LoadError::OpenFileFailed);
                }
                let file = File::open(&resolved).map_err(|_| tobj::LoadError::OpenFileFailed)?;
                let length = file
                    .metadata()
                    .map_err(|_| tobj::LoadError::OpenFileFailed)?
                    .len();
                let new_total = total_source_bytes
                    .get()
                    .checked_add(length)
                    .ok_or(tobj::LoadError::OpenFileFailed)?;
                if length > MAX_OBJ_SOURCE_BYTES || new_total > MAX_OBJ_SOURCE_BYTES {
                    return Err(tobj::LoadError::OpenFileFailed);
                }
                total_source_bytes.set(new_total);
                let mut reader = BufReader::new(file.take(MAX_OBJ_SOURCE_BYTES + 1));
                tobj::load_mtl_buf(&mut reader)
            },
        )
        .with_context(|| format!("Parsing OBJ {}", path.display()))?;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut materials: Vec<String> = source_materials
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, m)| {
                if m.name.trim().is_empty() {
                    format!("Material {}", index + 1)
                } else {
                    m.name.clone()
                }
            })
            .collect();
        let mut triangle_materials = Vec::new();
        for model in models {
            let m = model.mesh;
            let vertex_count = m.positions.len() / 3;
            check_import_growth(vertices.len(), indices.len(), vertex_count, m.indices.len())?;
            if !m.positions.len().is_multiple_of(3) {
                bail!("OBJ model contains an incomplete position coordinate");
            }
            validate_primitive_indices(&m.indices, vertex_count)?;
            let material_index = m
                .material_id
                .filter(|i| *i < materials.len())
                .map(|i| i as u32)
                .unwrap_or(u32::MAX);
            triangle_materials.extend(std::iter::repeat_n(material_index, m.indices.len() / 3));
            let offset = u32::try_from(vertices.len())
                .context("Imported OBJ vertex offset exceeds the supported index range")?;
            for i in 0..vertex_count {
                let pos = [
                    m.positions[i * 3],
                    m.positions[i * 3 + 1],
                    m.positions[i * 3 + 2],
                ];
                let normal = if m.normals.len() >= i * 3 + 3 {
                    [m.normals[i * 3], m.normals[i * 3 + 1], m.normals[i * 3 + 2]]
                } else {
                    [0.0; 3]
                };
                let uv = if m.texcoords.len() >= i * 2 + 2 {
                    [m.texcoords[i * 2], 1.0 - m.texcoords[i * 2 + 1]]
                } else {
                    [0.0, 0.0]
                };
                vertices.push(Vertex {
                    position: pos,
                    normal,
                    uv,
                    extensions: JsonMap::new(),
                });
            }
            indices.extend(m.indices.into_iter().map(|i| offset + i));
        }
        Self::finish_material_slots(&mut materials, &mut triangle_materials);
        let mut mesh = Self {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Imported Mesh".into()),
            vertices,
            indices,
            materials,
            triangle_materials,
            extensions: JsonMap::new(),
        };
        mesh.finish_import()?;
        Ok(mesh)
    }

    fn from_gltf(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening glTF {}", path.display()))?;
        let file_len = file.metadata()?.len();
        if file_len > MAX_GLTF_FILE_BYTES {
            bail!("glTF source exceeds the {MAX_GLTF_FILE_BYTES} byte safety limit");
        }
        // Parse the bounded document and binary payload without invoking gltf::import,
        // which also decodes every referenced image even though mesh import ignores them.
        let gltf = gltf::Gltf::from_reader(file)
            .with_context(|| format!("Parsing glTF {}", path.display()))?;
        let base = asset_base_dir(path)?;
        let gltf::Gltf { document, blob } = gltf;
        let buffers = read_gltf_buffers(&document, blob, &base)?;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut triangle_materials = Vec::new();
        let mut materials: Vec<String> = document
            .materials()
            .enumerate()
            .map(|(i, m)| {
                m.name()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Material {}", i + 1))
            })
            .collect();
        let scenes: Vec<_> = document
            .default_scene()
            .map(|s| vec![s])
            .unwrap_or_else(|| document.scenes().collect());
        for scene in scenes {
            for node in scene.nodes() {
                Self::read_gltf_node(
                    node,
                    1,
                    Mat4::IDENTITY,
                    &buffers,
                    &mut vertices,
                    &mut indices,
                    &mut triangle_materials,
                )?;
            }
        }
        Self::finish_material_slots(&mut materials, &mut triangle_materials);
        let mut mesh = Self {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Imported Mesh".into()),
            vertices,
            indices,
            materials,
            triangle_materials,
            extensions: JsonMap::new(),
        };
        mesh.finish_import()?;
        Ok(mesh)
    }

    fn read_gltf_node(
        node: gltf::Node<'_>,
        depth: usize,
        parent: Mat4,
        buffers: &[gltf::buffer::Data],
        vertices: &mut Vec<Vertex>,
        indices: &mut Vec<u32>,
        triangle_materials: &mut Vec<u32>,
    ) -> Result<()> {
        check_gltf_node_depth(depth)?;
        let transform = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                let reader = primitive.reader(|b| Some(&buffers[b.index()]));
                let Some(positions) = reader.read_positions() else {
                    continue;
                };
                let position_count = primitive
                    .get(&gltf::Semantic::Positions)
                    .map(|accessor| accessor.count())
                    .unwrap_or(0);
                let index_count = primitive
                    .indices()
                    .map(|accessor| accessor.count())
                    .unwrap_or(position_count);
                check_import_growth(vertices.len(), indices.len(), position_count, index_count)?;
                for semantic in [gltf::Semantic::Normals, gltf::Semantic::TexCoords(0)] {
                    if primitive
                        .get(&semantic)
                        .is_some_and(|accessor| accessor.count() != position_count)
                    {
                        bail!("glTF primitive attribute count does not match its positions");
                    }
                }
                let ps: Vec<_> = positions.collect();
                if ps.len() != position_count {
                    bail!("glTF position accessor count changed while reading primitive");
                }
                let ns: Vec<_> = reader
                    .read_normals()
                    .map(|iter| iter.collect())
                    .unwrap_or_default();
                let uv: Vec<_> = reader
                    .read_tex_coords(0)
                    .map(|c| c.into_f32().collect())
                    .unwrap_or_default();
                let ids: Vec<u32> = reader
                    .read_indices()
                    .map(|i| i.into_u32().collect())
                    .unwrap_or_else(|| (0..ps.len() as u32).collect());
                if ids.len() != index_count {
                    bail!("glTF index accessor count changed while reading primitive");
                }
                validate_primitive_indices(&ids, ps.len())?;
                let offset = u32::try_from(vertices.len())
                    .context("Imported glTF vertex offset exceeds the supported index range")?;
                let normal_transform = transform.inverse().transpose();
                for (i, pos) in ps.into_iter().enumerate() {
                    let p = transform.transform_point3(Vec3::from_array(pos));
                    let n = ns
                        .get(i)
                        .map(|n| {
                            normal_transform
                                .transform_vector3(Vec3::from_array(*n))
                                .normalize_or_zero()
                        })
                        .unwrap_or(Vec3::ZERO);
                    vertices.push(Vertex {
                        position: p.to_array(),
                        normal: n.to_array(),
                        uv: uv.get(i).copied().unwrap_or([0.0, 0.0]),
                        extensions: JsonMap::new(),
                    });
                }
                let material_index = primitive
                    .material()
                    .index()
                    .map(|i| i as u32)
                    .unwrap_or(u32::MAX);
                triangle_materials.extend(std::iter::repeat_n(material_index, ids.len() / 3));
                indices.extend(ids.into_iter().map(|i| offset + i));
            }
        }
        for child in node.children() {
            Self::read_gltf_node(
                child,
                depth + 1,
                transform,
                buffers,
                vertices,
                indices,
                triangle_materials,
            )?;
        }
        Ok(())
    }

    /// Uses the actual source material slots, adding a Default slot only if a
    /// triangle has no material assigned. u32::MAX is an internal import sentinel.
    fn finish_material_slots(materials: &mut Vec<String>, triangle_materials: &mut [u32]) {
        if materials.is_empty() || triangle_materials.contains(&u32::MAX) {
            let index = materials.len() as u32;
            materials.push("Default".to_owned());
            for slot in triangle_materials {
                if *slot == u32::MAX {
                    *slot = index;
                }
            }
        }
    }

    fn finish_import(&mut self) -> Result<()> {
        self.validate()?;
        if self
            .vertices
            .iter()
            .any(|v| Vec3::from_array(v.normal).length_squared() < 0.001)
        {
            for v in &mut self.vertices {
                v.normal = [0.0; 3];
            }
            for ids in self.indices.chunks_exact(3) {
                let (a, b, c) = (ids[0] as usize, ids[1] as usize, ids[2] as usize);
                let pa = Vec3::from_array(self.vertices[a].position);
                let pb = Vec3::from_array(self.vertices[b].position);
                let pc = Vec3::from_array(self.vertices[c].position);
                let n = (pb - pa).cross(pc - pa);
                for k in [a, b, c] {
                    let v = Vec3::from_array(self.vertices[k].normal) + n;
                    self.vertices[k].normal = v.to_array();
                }
            }
            for v in &mut self.vertices {
                v.normal = Vec3::from_array(v.normal).normalize_or_zero().to_array();
            }
        }
        // Normalize to a convenient viewport scale; the source asset is NOT overwritten.
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for v in &self.vertices {
            let p = Vec3::from_array(v.position);
            min = min.min(p);
            max = max.max(p);
        }
        let center = (min + max) * 0.5;
        let scale = 2.0 / (max - min).max_element().max(1e-6);
        for v in &mut self.vertices {
            v.position = ((Vec3::from_array(v.position) - center) * scale).to_array();
        }
        Ok(())
    }

    pub fn material_count(&self) -> usize {
        self.materials.len().max(1)
    }

    pub fn material_name(&self, slot: usize) -> &str {
        self.materials
            .get(slot)
            .map(String::as_str)
            .unwrap_or("Default")
    }

    pub fn triangle_material(&self, triangle: usize) -> usize {
        self.triangle_materials.get(triangle).copied().unwrap_or(0) as usize
    }

    pub fn validate(&self) -> Result<()> {
        if self.vertices.is_empty()
            || self.indices.is_empty()
            || !self.indices.len().is_multiple_of(3)
        {
            bail!("Model is empty or not made of triangles");
        }
        if !self.triangle_materials.is_empty()
            && self.triangle_materials.len() != self.indices.len() / 3
        {
            bail!("Material slot count must match triangle count");
        }
        if self.material_count() > 256 {
            bail!("Mesh exceeds 256 material slots");
        }
        if self
            .materials
            .iter()
            .any(|name| name.len() > 256 || name.contains('\0'))
        {
            bail!("Invalid mesh material name");
        }
        if self
            .triangle_materials
            .iter()
            .any(|slot| (*slot as usize) >= self.material_count())
        {
            bail!("Triangle references an unknown material slot");
        }
        if self.indices.len() / 3 > MAX_TRIANGLES {
            bail!("Model exceeds the {} triangle safety limit", MAX_TRIANGLES);
        }
        for i in &self.indices {
            if *i as usize >= self.vertices.len() {
                bail!("Invalid mesh index {i}");
            }
        }
        for v in &self.vertices {
            if v.position
                .iter()
                .chain(v.normal.iter())
                .chain(v.uv.iter())
                .any(|n| !n.is_finite())
            {
                bail!("Model has invalid float coordinates");
            }
        }
        Ok(())
    }
}

fn check_import_growth(
    current_vertices: usize,
    current_indices: usize,
    added_vertices: usize,
    added_indices: usize,
) -> Result<()> {
    let vertices = current_vertices
        .checked_add(added_vertices)
        .context("Imported mesh vertex count overflow")?;
    let indices = current_indices
        .checked_add(added_indices)
        .context("Imported mesh index count overflow")?;
    if vertices > MAX_IMPORTED_VERTICES {
        bail!("Imported mesh exceeds the {MAX_IMPORTED_VERTICES} vertex safety limit");
    }
    if indices > MAX_IMPORTED_INDICES {
        bail!(
            "Imported mesh exceeds the {MAX_IMPORTED_INDICES} index safety limit ({MAX_TRIANGLES} triangles)"
        );
    }
    if !indices.is_multiple_of(3) {
        bail!("Imported mesh primitive indices must form complete triangles");
    }
    Ok(())
}

fn validate_primitive_indices(indices: &[u32], vertex_count: usize) -> Result<()> {
    if !indices.len().is_multiple_of(3) {
        bail!("Imported mesh primitive indices must form complete triangles");
    }
    if let Some(index) = indices
        .iter()
        .find(|index| **index as usize >= vertex_count)
    {
        bail!("Imported mesh primitive references missing vertex {index}");
    }
    Ok(())
}

fn check_gltf_node_depth(depth: usize) -> Result<()> {
    if depth > MAX_GLTF_NODE_DEPTH {
        bail!("glTF node hierarchy exceeds the {MAX_GLTF_NODE_DEPTH}-node depth limit");
    }
    Ok(())
}

fn asset_base_dir(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.canonicalize()?)
}

fn read_gltf_buffers(
    document: &gltf::Document,
    mut blob: Option<Vec<u8>>,
    base: &Path,
) -> Result<Vec<gltf::buffer::Data>> {
    let mut total = 0usize;
    let mut buffers = Vec::new();
    for buffer in document.buffers() {
        let declared_len = buffer.length();
        if declared_len > MAX_GLTF_BUFFER_BYTES {
            bail!("glTF buffer exceeds the {MAX_GLTF_BUFFER_BYTES} byte safety limit");
        }
        total = total
            .checked_add(declared_len)
            .context("glTF buffer size overflow")?;
        if total > MAX_GLTF_BUFFER_BYTES {
            bail!("glTF buffers exceed the {MAX_GLTF_BUFFER_BYTES} byte aggregate safety limit");
        }

        let mut data = match buffer.source() {
            gltf::buffer::Source::Bin => blob
                .take()
                .context("glTF buffer refers to a missing GLB binary payload")?,
            gltf::buffer::Source::Uri(uri) => {
                if let Some(encoded) = uri.strip_prefix("data:") {
                    let (metadata, payload) =
                        encoded.split_once(',').context("Malformed glTF data URI")?;
                    if !metadata.ends_with(";base64") {
                        bail!("Only base64 glTF data URIs are supported for mesh buffers");
                    }
                    let max_encoded = declared_len.saturating_add(3).div_ceil(3).saturating_mul(4);
                    if payload.len() > max_encoded {
                        bail!("glTF data URI exceeds its declared buffer length");
                    }
                    base64::decode(payload).context("Decoding glTF data URI")?
                } else {
                    let decoded = urlencoding::decode(uri).context("Decoding glTF buffer URI")?;
                    let relative = Path::new(decoded.as_ref());
                    if relative
                        .components()
                        .any(|component| !matches!(component, Component::Normal(_)))
                    {
                        bail!("glTF external buffers must use paths within the asset directory");
                    }
                    let path = base.join(relative).canonicalize().with_context(|| {
                        format!("Resolving glTF external buffer {}", relative.display())
                    })?;
                    if !path.starts_with(base) {
                        bail!("glTF external buffer resolves outside the asset directory");
                    }
                    let file = File::open(&path)?;
                    let file_len = file.metadata()?.len();
                    if file_len > declared_len as u64 + 3 {
                        bail!("glTF external buffer exceeds its declared buffer length");
                    }
                    let mut bytes = Vec::with_capacity(file_len as usize);
                    file.take(declared_len as u64 + 4).read_to_end(&mut bytes)?;
                    bytes
                }
            }
        };

        if data.len() < declared_len || data.len() > declared_len.saturating_add(3) {
            bail!(
                "glTF buffer {} length does not match its declared length",
                buffer.index()
            );
        }
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        buffers.push(gltf::buffer::Data(data));
    }
    Ok(buffers)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cube_has_six_faces_and_uvs() {
        let mesh = Mesh::demo_cube();
        mesh.validate().unwrap();
        assert_eq!(mesh.indices.len(), 36);
        assert!(mesh
            .vertices
            .iter()
            .all(|v| v.uv.iter().all(|n| (0.0..=1.0).contains(n))));
    }
    #[test]
    fn obj_import_keeps_source_material_slots() {
        let dir = std::env::temp_dir().join(format!("kalium-mtl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj = dir.join("demo.obj");
        std::fs::write(
            dir.join("demo.mtl"),
            "newmtl Metal\nKd 0.5 0.5 0.5\nnewmtl Leather\nKd 0.4 0.3 0.2\n",
        )
        .unwrap();
        std::fs::write(&obj, "mtllib demo.mtl\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nusemtl Metal\nf 1/1 2/2 3/3\nusemtl Leather\nf 1/1 3/3 4/4\n").unwrap();
        let mesh = Mesh::from_file(&obj).unwrap();
        assert_eq!(
            mesh.materials,
            vec!["Metal".to_owned(), "Leather".to_owned()]
        );
        assert_eq!(mesh.triangle_materials, vec![0, 1]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn obj_import_caps_source_size_before_parsing() {
        let dir =
            std::env::temp_dir().join(format!("kalium-large-obj-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj = dir.join("large.obj");
        let file = File::create(&obj).unwrap();
        file.set_len(MAX_OBJ_SOURCE_BYTES + 1).unwrap();
        assert!(Mesh::from_file(&obj)
            .unwrap_err()
            .to_string()
            .contains("safety limit"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn obj_material_references_cannot_escape_asset_directory() {
        let parent =
            std::env::temp_dir().join(format!("kalium-obj-mtl-boundary-{}", std::process::id()));
        let dir = parent.join("asset");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(parent.join("outside.mtl"), "newmtl External\nKd 1 0 0\n").unwrap();
        let obj = dir.join("triangle.obj");
        std::fs::write(
            &obj,
            "mtllib ../outside.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nusemtl External\nf 1 2 3\n",
        )
        .unwrap();
        let mesh = Mesh::from_file(&obj).unwrap();
        assert!(!mesh.materials.iter().any(|material| material == "External"));
        std::fs::remove_dir_all(parent).ok();
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
        let mut mesh = Mesh::demo_cube();
        mesh.indices[0] = 999;
        assert!(mesh.validate().is_err());
    }

    #[test]
    fn imported_mesh_growth_enforces_vertex_and_index_limits() {
        assert!(check_import_growth(0, 0, MAX_IMPORTED_VERTICES, MAX_IMPORTED_INDICES).is_ok());
        assert!(check_import_growth(MAX_IMPORTED_VERTICES, 0, 1, 0)
            .unwrap_err()
            .to_string()
            .contains("vertex safety limit"));
        assert!(check_import_growth(0, MAX_IMPORTED_INDICES, 0, 3)
            .unwrap_err()
            .to_string()
            .contains("index safety limit"));
        assert!(check_import_growth(usize::MAX, 0, 1, 0).is_err());
    }

    #[test]
    fn gltf_node_traversal_depth_is_bounded() {
        assert!(check_gltf_node_depth(MAX_GLTF_NODE_DEPTH).is_ok());
        assert!(check_gltf_node_depth(MAX_GLTF_NODE_DEPTH + 1)
            .unwrap_err()
            .to_string()
            .contains("hierarchy"));
    }

    #[test]
    fn malformed_primitive_indices_are_rejected_before_offsetting() {
        assert!(validate_primitive_indices(&[0, 1, 2], 3).is_ok());
        assert!(validate_primitive_indices(&[0, 1, 3], 3)
            .unwrap_err()
            .to_string()
            .contains("missing vertex"));
        assert!(validate_primitive_indices(&[0, 1], 3)
            .unwrap_err()
            .to_string()
            .contains("complete triangles"));
    }

    #[test]
    fn gltf_buffer_data_uri_is_decoded_with_declared_length_checks() {
        let gltf = gltf::Gltf::from_slice(
            br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4,"uri":"data:application/octet-stream;base64,AQIDBA=="}]}"#,
        )
        .unwrap();
        let base = std::env::temp_dir();
        let buffers = read_gltf_buffers(&gltf.document, gltf.blob, &base).unwrap();
        assert_eq!(buffers.len(), 1);
        assert_eq!(&buffers[0][..4], &[1, 2, 3, 4]);

        let oversized = gltf::Gltf::from_slice(
            br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"data:application/octet-stream;base64,AQIDBAU="}]}"#,
        )
        .unwrap();
        assert!(read_gltf_buffers(&oversized.document, oversized.blob, &base).is_err());
    }

    #[test]
    fn gltf_external_buffers_cannot_escape_asset_directory() {
        let gltf = gltf::Gltf::from_slice(
            br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":1,"uri":"../outside.bin"}]}"#,
        )
        .unwrap();
        let base = std::env::temp_dir();
        assert!(read_gltf_buffers(&gltf.document, gltf.blob, &base)
            .unwrap_err()
            .to_string()
            .contains("within the asset directory"));
    }

    #[test]
    fn mesh_files_without_parent_component_use_current_directory() {
        assert_eq!(
            asset_base_dir(Path::new("relative.obj")).unwrap(),
            Path::new(".").canonicalize().unwrap()
        );
    }

    #[test]
    fn gltf_declared_buffer_allocation_is_bounded() {
        let gltf = gltf::Gltf::from_slice(
            format!(
                r#"{{"asset":{{"version":"2.0"}},"buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,"}}]}}"#,
                MAX_GLTF_BUFFER_BYTES + 1
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(read_gltf_buffers(&gltf.document, gltf.blob, Path::new(".")).is_err());
    }

    #[test]
    fn gltf_mesh_import_does_not_decode_unused_images() {
        let dir =
            std::env::temp_dir().join(format!("kalium-gltf-image-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("triangle.gltf");
        let mut positions = Vec::new();
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            positions.extend_from_slice(&value.to_le_bytes());
        }
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "buffers": [{
                "byteLength": positions.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&positions))
            }],
            "bufferViews": [{"buffer": 0, "byteLength": positions.len()}],
            "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]}],
            "images": [{"uri": "data:image/png;base64,not-an-image"}]
        });
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let mesh = Mesh::from_file(&path).unwrap();
        assert_eq!(mesh.vertices.len(), 3);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_node_translation_rotation_and_nonuniform_scale_are_baked() {
        let dir =
            std::env::temp_dir().join(format!("kalium-gltf-transform-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transformed.gltf");
        let mut positions = Vec::new();
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            positions.extend_from_slice(&value.to_le_bytes());
        }
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "translation": [2, 3, 0], "rotation": [0, 0, 0.70710677, 0.70710677], "scale": [2, 1, 1]}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "buffers": [{
                "byteLength": positions.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&positions))
            }],
            "bufferViews": [{"buffer": 0, "byteLength": positions.len()}],
            "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]}]
        });
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let mesh = Mesh::from_file(&path).unwrap();
        let expected = [[0.5, -1.0, 0.0], [0.5, 1.0, 0.0], [-0.5, -1.0, 0.0]];
        for (vertex, expected) in mesh.vertices.iter().zip(expected) {
            for (actual, expected) in vertex.position.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-5, "{actual} != {expected}");
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_nonuniform_scale_uses_inverse_transpose_for_normals() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-normal-transform-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nonuniform-normal.gltf");
        let inv_sqrt_2 = std::f32::consts::FRAC_1_SQRT_2;
        // The first edge is perpendicular to each source normal. Under scale
        // [2, 1, 1], the tangent becomes [-2, 1, 0]. The inverse-transpose
        // normal is proportional to [1, 2, 0], while a naive vector transform
        // would produce [2, 1, 0], which is not perpendicular to that tangent.
        let values = [
            0.0, 0.0, 0.0, // position 0
            -1.0, 1.0, 0.0, // position 1
            0.0, 0.0, 1.0, // position 2
            inv_sqrt_2, inv_sqrt_2, 0.0, // normal 0
            inv_sqrt_2, inv_sqrt_2, 0.0, // normal 1
            inv_sqrt_2, inv_sqrt_2, 0.0, // normal 2
        ];
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "scale": [2, 1, 1]}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1}}]}],
            "buffers": [{
                "byteLength": bytes.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&bytes))
            }],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36},
                {"buffer": 0, "byteOffset": 36, "byteLength": 36}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-1, 0, 0], "max": [0, 1, 1]},
                {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let mesh = Mesh::from_file(&path).unwrap();
        let normal = Vec3::from_array(mesh.vertices[0].normal);
        let tangent = Vec3::from_array(mesh.vertices[1].position)
            - Vec3::from_array(mesh.vertices[0].position);
        assert!(normal.is_finite());
        assert!((normal.length() - 1.0).abs() < 1e-6);
        assert!((normal.dot(tangent)).abs() < 1e-6);
        assert!((normal.x - 1.0 / 5.0_f32.sqrt()).abs() < 1e-6);
        assert!((normal.y - 2.0 / 5.0_f32.sqrt()).abs() < 1e-6);

        std::fs::remove_dir_all(dir).ok();
    }
}

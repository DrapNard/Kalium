use anyhow::{bail, Context, Result};
use glam::{Mat4, Vec3, Vec4};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

pub const MAX_TRIANGLES: usize = 350_000;
// These limits bound the in-memory mesh assembled by this module. Format
// decoders may allocate source buffers before these traversal limits apply.
const MAX_IMPORTED_VERTICES: usize = 1_050_000;
const MAX_IMPORTED_INDICES: usize = MAX_TRIANGLES * 3;
const MAX_GLTF_NODE_DEPTH: usize = 256;
const MAX_GLTF_NODE_VISITS: usize = 100_000;
const MAX_GLTF_NODES: usize = 100_000;
const MAX_GLTF_MATERIALS: usize = MAX_MTL_MATERIALS;
const MAX_GLTF_UV_SETS: usize = 8;
const MAX_GLTF_UV_SET_INDEX: u32 = MAX_GLTF_UV_SETS as u32 - 1;
const MAX_GLTF_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_GLTF_BUFFER_BYTES: usize = 128 * 1024 * 1024;
const MAX_OBJ_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_OBJ_LINE_BYTES: usize = 1024 * 1024;
const MAX_OBJ_MODEL_GROUPS: usize = 100_000;
const MAX_OBJ_MTL_LIBRARIES: usize = 256;
const MAX_MTL_MATERIALS: usize = 256;
const MAX_MTL_UNKNOWN_PARAMS_PER_MATERIAL: usize = 64;
const MAX_MTL_UNKNOWN_PARAMS_TOTAL: usize = 4096;
const MAX_MTL_UNKNOWN_KEY_BYTES: usize = 128;
const MAX_MTL_UNKNOWN_VALUE_BYTES: usize = 4096;
const MAX_MTL_UNKNOWN_BYTES_TOTAL: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MtlPreflightSummary {
    material_count: usize,
    unknown_param_count: usize,
    unknown_param_bytes: usize,
}

#[derive(Clone, Copy)]
struct ObjPreflightLimits {
    source_bytes: u64,
    line_bytes: usize,
    vertices: usize,
    face_corners: usize,
    indices: usize,
    model_groups: usize,
}

struct GltfTraversal<'a> {
    buffers: &'a [gltf::buffer::Data],
    node_visits: usize,
}

#[derive(Default)]
struct GltfMeshParts {
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    triangle_materials: Vec<u32>,
    extra_uv_sets: BTreeMap<u32, Vec<Option<[f32; 2]>>>,
}

const OBJ_PREFLIGHT_LIMITS: ObjPreflightLimits = ObjPreflightLimits {
    source_bytes: MAX_OBJ_SOURCE_BYTES,
    line_bytes: MAX_OBJ_LINE_BYTES,
    vertices: MAX_IMPORTED_VERTICES,
    face_corners: MAX_IMPORTED_VERTICES,
    indices: MAX_IMPORTED_INDICES,
    model_groups: MAX_OBJ_MODEL_GROUPS,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    /// Top-left origin: [0, 0] is the top-left corner of a texture.
    pub uv: [f32; 2],
    /// Optional tangent direction and bitangent handedness from the source mesh.
    /// XYZ is the tangent and W is the sign used to construct the bitangent.
    #[serde(default)]
    pub tangent: Option<[f32; 4]>,
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
    /// Additional glTF texture-coordinate sets keyed by their TEXCOORD_n index.
    /// Each vector is aligned with `vertices`; `None` means the source primitive
    /// did not provide that set. Set 0 remains in `Vertex::uv` for compatibility.
    #[serde(default)]
    pub extra_uv_sets: BTreeMap<u32, Vec<Option<[f32; 2]>>>,
    /// Unknown mesh-manifest fields retained for forward-compatible project round trips.
    #[serde(flatten)]
    pub(crate) extensions: JsonMap<String, JsonValue>,
}

/// Matching geometric edge endpoints and their UV coordinates in two source
/// triangles. Vertex indices may differ when a file duplicates vertices at a
/// UV seam; positions may differ by at most a small, scale-relative tolerance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SharedTriangleEdge {
    pub positions: [[f32; 3]; 2],
    pub first_uv: [[f32; 2]; 2],
    pub second_uv: [[f32; 2]; 2],
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
                    tangent: None,
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
            extra_uv_sets: Default::default(),
            extensions: JsonMap::new(),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        Self::from_file_with_normalization(path, true)
    }

    /// Imports an OBJ/glTF mesh while retaining its coordinates after any
    /// glTF node transforms. This is intended for operations that need to map
    /// imported geometry into another mesh's coordinate space.
    pub fn from_file_preserving_coordinates(path: &Path) -> Result<Self> {
        Self::from_file_with_normalization(path, false)
    }

    fn from_file_with_normalization(path: &Path, normalize: bool) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        match extension.as_str() {
            "obj" => Self::from_obj(path, normalize),
            "gltf" | "glb" => Self::from_gltf(path, normalize),
            _ => bail!("Unsupported mesh format. Use .obj, .gltf or .glb"),
        }
    }

    fn from_obj(path: &Path, normalize: bool) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
        let obj_len = file.metadata()?.len();
        if obj_len > MAX_OBJ_SOURCE_BYTES {
            bail!("OBJ source exceeds the {MAX_OBJ_SOURCE_BYTES} byte safety limit");
        }
        let base = asset_base_dir(path)?;
        let total_source_bytes = Cell::new(obj_len);
        let total_mtl_summary = Cell::new(MtlPreflightSummary::default());
        let mtl_load_error = std::cell::RefCell::new(None::<String>);
        let mut reader = BufReader::new(file);
        preflight_obj_reader(&mut reader, OBJ_PREFLIGHT_LIMITS)?;
        reader.seek(SeekFrom::Start(0))?;
        let mut bounded_reader = (&mut reader).take(MAX_OBJ_SOURCE_BYTES + 1);
        let (models, source_materials) = tobj::load_obj_buf(
            &mut bounded_reader,
            &tobj::LoadOptions {
                triangulate: true,
                single_index: true,
                ..Default::default()
            },
            |material_path| {
                let loaded = (|| -> std::result::Result<tobj::MTLLoadResult, String> {
                    let resolved = base.join(material_path).canonicalize().map_err(|error| {
                        format!(
                            "Could not resolve referenced material library {}: {error}",
                            material_path.display()
                        )
                    })?;
                    if !resolved.starts_with(&base) {
                        return Err(format!(
                            "Referenced material library {} resolves outside the asset directory",
                            material_path.display()
                        ));
                    }
                    let file = File::open(&resolved).map_err(|error| {
                        format!(
                            "Could not open referenced material library {}: {error}",
                            material_path.display()
                        )
                    })?;
                    let length = file.metadata().map_err(|error| {
                        format!(
                            "Could not inspect referenced material library {}: {error}",
                            material_path.display()
                        )
                    })?.len();
                    let new_total = total_source_bytes
                        .get()
                        .checked_add(length)
                        .ok_or_else(|| "OBJ and MTL source length overflow".to_owned())?;
                    if length > MAX_OBJ_SOURCE_BYTES || new_total > MAX_OBJ_SOURCE_BYTES {
                        return Err(format!(
                            "Referenced material library {} exceeds the OBJ/MTL source size safety limit",
                            material_path.display()
                        ));
                    }
                    total_source_bytes.set(new_total);
                    let mut reader = BufReader::new(file);
                    let summary = preflight_mtl_reader(
                        &mut reader,
                        MAX_OBJ_SOURCE_BYTES,
                        MAX_OBJ_LINE_BYTES,
                    )
                    .map_err(|error| {
                        format!(
                            "Invalid referenced material library {}: {error:#}",
                            material_path.display()
                        )
                    })?;
                    let total_summary = combine_mtl_preflight_summaries(
                        total_mtl_summary.get(),
                        summary,
                    )
                    .map_err(|error| {
                        format!(
                            "Invalid referenced material library {}: {error:#}",
                            material_path.display()
                        )
                    })?;
                    total_mtl_summary.set(total_summary);
                    reader.seek(SeekFrom::Start(0)).map_err(|error| {
                        format!(
                            "Could not reread referenced material library {}: {error}",
                            material_path.display()
                        )
                    })?;
                    let mut bounded_reader = (&mut reader).take(MAX_OBJ_SOURCE_BYTES + 1);
                    let materials = tobj::load_mtl_buf(&mut bounded_reader).map_err(|error| {
                        format!(
                            "Could not parse referenced material library {}: {error:?}",
                            material_path.display()
                        )
                    })?;
                    Ok(Ok(materials))
                })();
                match loaded {
                    Ok(materials) => materials,
                    Err(error) => {
                        let mut recorded = mtl_load_error.borrow_mut();
                        if recorded.is_none() {
                            *recorded = Some(error);
                        }
                        Err(tobj::LoadError::OpenFileFailed)
                    }
                }
            },
        )
        .with_context(|| format!("Parsing OBJ {}", path.display()))?;
        if let Some(error) = mtl_load_error.into_inner() {
            bail!("Failed to load an OBJ material library: {error}");
        }
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut materials: Vec<String> = source_materials
            .context("Loading OBJ material libraries")?
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
            let expected_texcoord_count = vertex_count
                .checked_mul(2)
                .context("OBJ texture-coordinate count overflow")?;
            if m.texcoords.len() != expected_texcoord_count {
                bail!(
                    "OBJ model {:?} is missing complete texture coordinates (vt); add UVs before importing this mesh",
                    model.name
                );
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
                let uv = [m.texcoords[i * 2], 1.0 - m.texcoords[i * 2 + 1]];
                vertices.push(Vertex {
                    position: pos,
                    normal,
                    uv,
                    tangent: None,
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
            extra_uv_sets: BTreeMap::new(),
            extensions: JsonMap::new(),
        };
        mesh.finish_import(normalize)?;
        Ok(mesh)
    }

    fn from_gltf(path: &Path, normalize: bool) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening glTF {}", path.display()))?;
        let file_len = file.metadata()?.len();
        if file_len > MAX_GLTF_FILE_BYTES {
            bail!("glTF source exceeds the {MAX_GLTF_FILE_BYTES} byte safety limit");
        }
        // Parse the bounded document and binary payload without invoking gltf::import,
        // which also decodes every referenced image even though mesh import ignores them.
        let gltf = parse_gltf_reader(file, MAX_GLTF_FILE_BYTES).map_err(|error| {
            if error
                .chain()
                .any(|cause| cause.to_string().contains("node document limit"))
            {
                error
            } else {
                error.context(format!("Parsing glTF {}", path.display()))
            }
        })?;
        let base = asset_base_dir(path)?;
        let gltf::Gltf { document, blob } = gltf;
        check_gltf_material_count(document.materials().count())?;
        check_gltf_node_count(document.nodes().count())?;
        let buffers = read_gltf_buffers(&document, blob, &base)?;
        let mut traversal = GltfTraversal {
            buffers: &buffers,
            node_visits: 0,
        };
        let mut parts = GltfMeshParts::default();
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
        // A glTF scene is an alternative view of the node graph, not an
        // additional mesh contribution. When the asset omits its default
        // scene, choose the first declared scene deterministically instead of
        // merging every alternative (which can duplicate shared nodes).
        let scenes: Vec<_> = if let Some(scene) = document.default_scene() {
            vec![scene]
        } else if let Some(first_scene) = document.scenes().next() {
            vec![first_scene]
        } else {
            Vec::new()
        };
        for scene in scenes {
            for node in scene.nodes() {
                Self::read_gltf_node(node, 1, Mat4::IDENTITY, &mut traversal, &mut parts)?;
            }
        }
        let GltfMeshParts {
            vertices,
            indices,
            mut triangle_materials,
            extra_uv_sets,
        } = parts;
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
            extra_uv_sets,
            extensions: JsonMap::new(),
        };
        mesh.finish_import(normalize)?;
        Ok(mesh)
    }

    fn read_gltf_node(
        node: gltf::Node<'_>,
        depth: usize,
        parent: Mat4,
        traversal: &mut GltfTraversal<'_>,
        parts: &mut GltfMeshParts,
    ) -> Result<()> {
        let GltfMeshParts {
            vertices,
            indices,
            triangle_materials,
            extra_uv_sets,
        } = parts;
        check_gltf_node_depth(depth)?;
        check_gltf_node_visit(&mut traversal.node_visits)?;
        let transform = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        if node.skin().is_some() {
            bail!("glTF skinning is not supported; export a static mesh before importing");
        }
        if node.weights().is_some() {
            bail!("glTF morph weights are not supported; export a static mesh before importing");
        }
        let determinant = transform.determinant();
        if !determinant.is_finite() || determinant == 0.0 {
            bail!("glTF node transform is singular or non-finite");
        }
        let normal_transform = transform.inverse().transpose();
        if !normal_transform.is_finite() {
            bail!("glTF node transform cannot safely transform normals");
        }
        let reflected = determinant < 0.0;
        if let Some(mesh) = node.mesh() {
            if mesh.weights().is_some() {
                bail!(
                    "glTF morph weights are not supported; export a static mesh before importing"
                );
            }
            for (primitive_index, primitive) in mesh.primitives().enumerate() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                if primitive.morph_targets().next().is_some() {
                    bail!("glTF morph targets are not supported; export a static mesh before importing");
                }
                let reader = primitive.reader(|b| Some(&traversal.buffers[b.index()]));
                let Some(positions) = reader.read_positions() else {
                    continue;
                };
                let position_count = primitive
                    .get(&gltf::Semantic::Positions)
                    .map(|accessor| accessor.count())
                    .unwrap_or(0);
                let mut uv_set_indices = Vec::new();
                for (semantic, _) in primitive.attributes() {
                    match semantic {
                        gltf::Semantic::TexCoords(set) => uv_set_indices.push(set),
                        gltf::Semantic::Joints(_) | gltf::Semantic::Weights(_) => bail!(
                            "glTF skinning attributes are not supported; export a static mesh before importing"
                        ),
                        _ => {}
                    }
                }
                if !uv_set_indices.contains(&0) {
                    bail!(
                        "glTF mesh {} primitive {primitive_index} is missing required TEXCOORD_0; add UVs before importing this mesh",
                        mesh.index()
                    );
                }
                uv_set_indices.sort_unstable();
                if uv_set_indices.len() > MAX_GLTF_UV_SETS
                    || uv_set_indices
                        .iter()
                        .any(|set| *set > MAX_GLTF_UV_SET_INDEX)
                {
                    bail!(
                        "glTF primitive exceeds Kalium's supported TEXCOORD_0 through TEXCOORD_{} range",
                        MAX_GLTF_UV_SET_INDEX
                    );
                }
                for set in &uv_set_indices {
                    if primitive
                        .get(&gltf::Semantic::TexCoords(*set))
                        .is_some_and(|accessor| accessor.count() != position_count)
                    {
                        bail!("glTF primitive TEXCOORD_{set} count does not match its positions");
                    }
                }
                let new_uv_sets = uv_set_indices
                    .iter()
                    .copied()
                    .filter(|set| *set != 0 && !extra_uv_sets.contains_key(set))
                    .count();
                if extra_uv_sets.len() + new_uv_sets >= MAX_GLTF_UV_SETS {
                    bail!("glTF scene exceeds Kalium's supported texture-coordinate set limit");
                }
                let index_count = primitive
                    .indices()
                    .map(|accessor| accessor.count())
                    .unwrap_or(position_count);
                check_import_growth(vertices.len(), indices.len(), position_count, index_count)?;
                for semantic in [gltf::Semantic::Normals] {
                    if primitive
                        .get(&semantic)
                        .is_some_and(|accessor| accessor.count() != position_count)
                    {
                        bail!("glTF primitive attribute count does not match its positions");
                    }
                }
                if primitive
                    .get(&gltf::Semantic::Tangents)
                    .is_some_and(|accessor| accessor.count() != position_count)
                {
                    bail!("glTF TANGENT accessor count does not match its positions");
                }
                let ps: Vec<_> = positions.collect();
                if ps.len() != position_count {
                    bail!("glTF position accessor count changed while reading primitive");
                }
                let ns: Vec<_> = reader
                    .read_normals()
                    .map(|iter| iter.collect())
                    .unwrap_or_default();
                let tangents: Vec<[f32; 4]> = reader
                    .read_tangents()
                    .map(|iter| iter.collect())
                    .unwrap_or_default();
                if !tangents.is_empty() && ns.len() != position_count {
                    bail!("glTF TANGENT attributes require matching NORMAL attributes");
                }
                let uv_values: BTreeMap<u32, Vec<[f32; 2]>> = uv_set_indices
                    .iter()
                    .map(|&set| {
                        let values = reader
                            .read_tex_coords(set)
                            .map(|coords| coords.into_f32().collect())
                            .unwrap_or_default();
                        (set, values)
                    })
                    .collect();
                for (&set, values) in &uv_values {
                    if values.len() != position_count {
                        bail!("glTF TEXCOORD_{set} accessor count changed while reading primitive");
                    }
                }
                let uv = uv_values
                    .get(&0)
                    .context("glTF TEXCOORD_0 accessor was not read")?;
                let vertex_offset = vertices.len();
                for (&set, _) in uv_values.iter().filter(|(set, _)| **set != 0) {
                    extra_uv_sets
                        .entry(set)
                        .or_insert_with(|| vec![None; vertex_offset]);
                }
                for values in extra_uv_sets.values_mut() {
                    values.reserve(ps.len());
                }
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
                    let tangent = tangents
                        .get(i)
                        .map(|tangent| {
                            transform_gltf_tangent(
                                *tangent,
                                transform,
                                normal_transform,
                                ns.get(i).copied(),
                                reflected,
                            )
                        })
                        .transpose()?;
                    vertices.push(Vertex {
                        position: p.to_array(),
                        normal: n.to_array(),
                        uv: *uv.get(i).context(
                            "glTF TEXCOORD_0 accessor count changed while reading primitive",
                        )?,
                        tangent,
                        extensions: JsonMap::new(),
                    });
                    for (&set, values) in extra_uv_sets.iter_mut() {
                        values.push(uv_values.get(&set).map(|set_values| set_values[i]));
                    }
                }
                let material_index = primitive
                    .material()
                    .index()
                    .map(|i| i as u32)
                    .unwrap_or(u32::MAX);
                triangle_materials.extend(std::iter::repeat_n(material_index, ids.len() / 3));
                if reflected {
                    for triangle in ids.chunks_exact(3) {
                        indices.extend([
                            offset + triangle[0],
                            offset + triangle[2],
                            offset + triangle[1],
                        ]);
                    }
                } else {
                    indices.extend(ids.into_iter().map(|i| offset + i));
                }
            }
        }
        for child in node.children() {
            Self::read_gltf_node(child, depth + 1, transform, traversal, parts)?;
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

    fn finish_import(&mut self, normalize: bool) -> Result<()> {
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
        if !normalize {
            return Ok(());
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

    /// Find a shared geometric edge between two source triangles, including
    /// duplicated vertices whose positions differ slightly at a UV seam.
    ///
    /// Correspondence uses a per-coordinate tolerance of
    /// (1e-4 + f32::EPSILON) times the larger triangle's largest axis extent.
    /// The epsilon term covers coordinate rounding at the threshold. Every
    /// matched corner must have exactly one counterpart; ambiguous
    /// near-coincident geometry is rejected.
    pub fn shared_triangle_edge(
        &self,
        first_triangle: usize,
        second_triangle: usize,
    ) -> Option<SharedTriangleEdge> {
        if first_triangle == second_triangle {
            return None;
        }
        let first = self
            .indices
            .get(first_triangle.checked_mul(3)?..first_triangle.checked_mul(3)?.checked_add(3)?)?;
        let second = self.indices.get(
            second_triangle.checked_mul(3)?..second_triangle.checked_mul(3)?.checked_add(3)?,
        )?;
        let first_positions = first
            .iter()
            .map(|index| {
                Some(Vec3::from_array(
                    self.vertices.get(*index as usize)?.position,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let second_positions = second
            .iter()
            .map(|index| {
                Some(Vec3::from_array(
                    self.vertices.get(*index as usize)?.position,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let triangle_extent = |positions: &[Vec3]| {
            let mut min = Vec3::splat(f32::INFINITY);
            let mut max = Vec3::splat(f32::NEG_INFINITY);
            for position in positions {
                if !position.is_finite() {
                    return None;
                }
                min = min.min(*position);
                max = max.max(*position);
            }
            Some((max - min).max_element())
        };
        let scale = triangle_extent(&first_positions)?.max(triangle_extent(&second_positions)?);
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let tolerance = scale * (1.0e-4 + f32::EPSILON);
        let mut matches = [(0usize, 0usize); 3];
        let mut match_count = 0usize;
        for (first_corner, first_position) in first_positions.iter().enumerate() {
            let mut candidate = None;
            for (second_corner, second_position) in second_positions.iter().enumerate() {
                if (*first_position - *second_position).abs().max_element() <= tolerance {
                    // Multiple candidates make the geometric correspondence
                    // underdetermined, even if a later corner could disambiguate it.
                    if candidate.replace(second_corner).is_some() {
                        return None;
                    }
                }
            }
            if let Some(second_corner) = candidate {
                if matches[..match_count]
                    .iter()
                    .any(|(_, existing_second)| *existing_second == second_corner)
                {
                    return None;
                }
                if match_count == matches.len() {
                    return None;
                }
                matches[match_count] = (first_corner, second_corner);
                match_count += 1;
            }
        }
        if match_count != 2 {
            return None;
        }
        let (first_a, second_a) = matches[0];
        let (first_b, second_b) = matches[1];
        let first_vertex_a = &self.vertices[first[first_a] as usize];
        let first_vertex_b = &self.vertices[first[first_b] as usize];
        if (Vec3::from_array(first_vertex_a.position) - Vec3::from_array(first_vertex_b.position))
            .abs()
            .max_element()
            <= tolerance
        {
            return None;
        }
        let second_vertex_a = &self.vertices[second[second_a] as usize];
        let second_vertex_b = &self.vertices[second[second_b] as usize];
        Some(SharedTriangleEdge {
            positions: [first_vertex_a.position, first_vertex_b.position],
            first_uv: [first_vertex_a.uv, first_vertex_b.uv],
            second_uv: [second_vertex_a.uv, second_vertex_b.uv],
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.vertices.is_empty()
            || self.indices.is_empty()
            || !self.indices.len().is_multiple_of(3)
        {
            bail!("Model is empty or not made of triangles");
        }
        validate_vertex_count(self.vertices.len())?;
        if !self.triangle_materials.is_empty()
            && self.triangle_materials.len() != self.indices.len() / 3
        {
            bail!("Material slot count must match triangle count");
        }
        if self.material_count() > 256 {
            bail!("Mesh exceeds 256 material slots");
        }
        if self.extra_uv_sets.len() >= MAX_GLTF_UV_SETS
            || self.extra_uv_sets.iter().any(|(set, values)| {
                *set == 0
                    || *set > MAX_GLTF_UV_SET_INDEX
                    || values.len() != self.vertices.len()
                    || values
                        .iter()
                        .flatten()
                        .flatten()
                        .any(|value| !value.is_finite())
            })
        {
            bail!("Mesh has invalid or unsupported additional UV sets");
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
            if let Some(tangent) = v.tangent {
                let direction = Vec3::new(tangent[0], tangent[1], tangent[2]);
                let normal = Vec3::from_array(v.normal).normalize_or_zero();
                if !direction.is_finite()
                    || (direction.length() - 1.0).abs() > 1.0e-3
                    || !tangent[3].is_finite()
                    || (tangent[3].abs() - 1.0).abs() > 1.0e-3
                    || normal.length_squared() <= f32::EPSILON
                    || direction.dot(normal).abs() > 1.0e-3
                {
                    bail!("Mesh tangent is invalid or not orthogonal to its normal");
                }
            }
        }
        Ok(())
    }
}

fn transform_gltf_tangent(
    tangent: [f32; 4],
    transform: Mat4,
    normal_transform: Mat4,
    source_normal: Option<[f32; 3]>,
    reflected: bool,
) -> Result<[f32; 4]> {
    let source_tangent = Vec3::new(tangent[0], tangent[1], tangent[2]);
    let handedness = tangent[3];
    if !source_tangent.is_finite()
        || source_tangent.length_squared() <= f32::EPSILON
        || !handedness.is_finite()
        || (handedness.abs() - 1.0).abs() > 1.0e-5
    {
        bail!("glTF TANGENT contains a non-finite, degenerate, or invalid value");
    }
    let source_normal = source_normal.context("glTF TANGENT is missing its NORMAL value")?;
    let source_normal = Vec3::from_array(source_normal);
    if !source_normal.is_finite() || source_normal.length_squared() <= f32::EPSILON {
        bail!("glTF TANGENT cannot be orthogonalized against a degenerate NORMAL");
    }
    let normal = normal_transform
        .transform_vector3(source_normal)
        .normalize_or_zero();
    let transformed = transform.transform_vector3(source_tangent);
    if !normal.is_finite() || normal.length_squared() <= f32::EPSILON || !transformed.is_finite() {
        bail!("glTF TANGENT transform is non-finite or degenerate");
    }
    let orthogonal = transformed - normal * transformed.dot(normal);
    if !orthogonal.is_finite() || orthogonal.length_squared() <= f32::EPSILON {
        bail!("glTF TANGENT becomes degenerate after transformation");
    }
    let direction = orthogonal.normalize();
    let handedness = if reflected { -handedness } else { handedness };
    Ok(Vec4::new(direction.x, direction.y, direction.z, handedness).to_array())
}

/// Parse a glTF document through a hard byte limit. Metadata checks avoid
/// reading obviously oversized regular files, while this reader cap also
/// protects against a file changing after metadata was inspected.
fn parse_gltf_reader<R: Read + Seek>(reader: R, max_bytes: u64) -> Result<gltf::Gltf> {
    let mut reader = reader;
    let preflight = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        preflight_gltf_node_count(&mut reader, max_bytes)
    }))
    .map_err(|_| anyhow::anyhow!("glTF preflight rejected malformed input"))?;
    preflight?;
    let read_limit = max_bytes
        .checked_add(1)
        .context("glTF byte limit overflow")?;
    let mut bounded = reader.take(read_limit);
    // gltf-json has historically indexed some schema arrays before checking
    // their lengths. Treat a library panic on an untrusted document as a parse
    // error at this boundary so malformed files cannot unwind through the app.
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gltf::Gltf::from_reader(&mut bounded)
    }))
    .map_err(|_| anyhow::anyhow!("glTF parser rejected malformed input"))?;
    let document = parsed.context("Parsing bounded glTF input")?;
    if bounded.limit() == 0 {
        bail!("glTF source exceeds the {max_bytes} byte safety limit");
    }
    Ok(document)
}

/// Stream the document's top-level `nodes` array before gltf-json builds its
/// schema vectors. `IgnoredAny` skips each node record without retaining it.
fn preflight_gltf_node_count<R: Read + Seek>(reader: &mut R, max_bytes: u64) -> Result<()> {
    let original_position = reader.stream_position()?;
    let result = (|| {
        let file_len = reader.seek(SeekFrom::End(0))?;
        if file_len > max_bytes {
            bail!("glTF source exceeds the {max_bytes} byte safety limit");
        }

        reader.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 4];
        let count = reader.read(&mut magic)?;
        let (json_start, json_len) = if count == magic.len() && &magic == b"glTF" {
            // GLB's JSON document is the first chunk. Leave malformed headers
            // to gltf::Gltf, which reports the format error after this check.
            let mut header = [0; 8];
            reader.seek(SeekFrom::Start(12))?;
            if reader.read_exact(&mut header).is_err() {
                return Ok(());
            }
            let chunk_len = u32::from_le_bytes(header[..4].try_into().unwrap()) as u64;
            let chunk_type = u32::from_le_bytes(header[4..].try_into().unwrap());
            let json_start = 20u64;
            if chunk_type != 0x4e4f_534a
                || chunk_len > max_bytes
                || json_start
                    .checked_add(chunk_len)
                    .is_none_or(|end| end > file_len)
            {
                return Ok(());
            }
            (json_start, chunk_len)
        } else {
            (0, file_len)
        };

        reader.seek(SeekFrom::Start(json_start))?;
        let mut deserializer = serde_json::Deserializer::from_reader(reader.take(json_len));
        GltfNodePreflight::deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(())
    })();
    reader.seek(SeekFrom::Start(original_position))?;
    result
}

struct GltfNodePreflight;

impl<'de> Deserialize<'de> for GltfNodePreflight {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DocumentVisitor;

        impl<'de> Visitor<'de> for DocumentVisitor {
            type Value = GltfNodePreflight;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a glTF JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while let Some(key) = map.next_key::<String>()? {
                    if key == "nodes" {
                        map.next_value_seed(GltfNodeArraySeed)?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(GltfNodePreflight)
            }
        }

        deserializer.deserialize_map(DocumentVisitor)
    }
}

struct GltfNodeArraySeed;

impl<'de> DeserializeSeed<'de> for GltfNodeArraySeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct NodeArrayVisitor;

        impl<'de> Visitor<'de> for NodeArrayVisitor {
            type Value = ();

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounded glTF nodes array")
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut count = 0usize;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    count += 1;
                    if count > MAX_GLTF_NODES {
                        return Err(serde::de::Error::custom(format!(
                            "glTF exceeds the {MAX_GLTF_NODES}-node document limit"
                        )));
                    }
                }
                Ok(())
            }
        }

        deserializer.deserialize_seq(NodeArrayVisitor)
    }
}

fn read_bounded_text_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    line_limit: usize,
    source_limit: u64,
    bytes_read: &mut u64,
) -> Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let line_size = line
            .len()
            .checked_add(count)
            .context("Text line length overflow")?;
        if line_size > line_limit {
            bail!("Text line exceeds the {line_limit} byte safety limit");
        }
        let next_bytes = bytes_read
            .checked_add(count as u64)
            .context("Text source length overflow")?;
        if next_bytes > source_limit {
            bail!("Text source exceeds the {source_limit} byte safety limit");
        }
        line.try_reserve(count)
            .context("Could not reserve bounded text line storage")?;
        line.extend_from_slice(&available[..count]);
        let line_ended = available[count - 1] == b'\n';
        reader.consume(count);
        *bytes_read = next_bytes;
        if line_ended {
            return Ok(true);
        }
    }
}

fn preflight_obj_reader<R: BufRead>(reader: &mut R, limits: ObjPreflightLimits) -> Result<()> {
    let mut line = Vec::new();
    let mut bytes_read = 0;
    let mut position_count = 0usize;
    let mut texcoord_count = 0usize;
    let mut normal_count = 0usize;
    let mut face_corner_count = 0usize;
    let mut index_count = 0usize;
    let mut model_group_count = 0usize;
    let mut mtl_library_count = 0usize;

    while read_bounded_text_line(
        reader,
        &mut line,
        limits.line_bytes,
        limits.source_bytes,
        &mut bytes_read,
    )? {
        let content = line.split(|byte| *byte == b'#').next().unwrap_or_default();
        let mut tokens = content
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|token| !token.is_empty());
        match tokens.next().unwrap_or_default() {
            b"v" => {
                position_count = position_count
                    .checked_add(1)
                    .context("OBJ position count overflow")?;
                if position_count > limits.vertices {
                    bail!("OBJ exceeds the {} source position limit", limits.vertices);
                }
            }
            b"vt" => {
                texcoord_count = texcoord_count
                    .checked_add(1)
                    .context("OBJ texture-coordinate count overflow")?;
                if texcoord_count > limits.vertices {
                    bail!(
                        "OBJ exceeds the {} source texture-coordinate limit",
                        limits.vertices
                    );
                }
            }
            b"vn" => {
                normal_count = normal_count
                    .checked_add(1)
                    .context("OBJ normal count overflow")?;
                if normal_count > limits.vertices {
                    bail!("OBJ exceeds the {} source normal limit", limits.vertices);
                }
            }
            b"f" => {
                let corners = tokens.count();
                face_corner_count = face_corner_count
                    .checked_add(corners)
                    .context("OBJ face-corner count overflow")?;
                if face_corner_count > limits.face_corners {
                    bail!(
                        "OBJ exceeds the {} face-corner safety limit",
                        limits.face_corners
                    );
                }
                // tobj triangulates one- and two-corner `f` records as
                // degenerate point/line triangles unless those primitives
                // are explicitly ignored. Account for the actual expansion
                // before handing input to the parser.
                let triangulated_indices = match corners {
                    0 => 0,
                    1 | 2 => 3,
                    _ => corners
                        .checked_sub(2)
                        .and_then(|triangles| triangles.checked_mul(3))
                        .context("OBJ triangulated index count overflow")?,
                };
                index_count = index_count
                    .checked_add(triangulated_indices)
                    .context("OBJ index count overflow")?;
                if index_count > limits.indices {
                    bail!(
                        "OBJ exceeds the {} triangulated index safety limit",
                        limits.indices
                    );
                }
            }
            b"l" => {
                let corners = tokens.count();
                face_corner_count = face_corner_count
                    .checked_add(corners)
                    .context("OBJ face-corner count overflow")?;
                if face_corner_count > limits.face_corners {
                    bail!(
                        "OBJ exceeds the {} face-corner safety limit",
                        limits.face_corners
                    );
                }
            }
            b"o" | b"g" | b"usemtl" => {
                model_group_count = model_group_count
                    .checked_add(1)
                    .context("OBJ model-group count overflow")?;
                if model_group_count > limits.model_groups {
                    bail!(
                        "OBJ exceeds the {} object/group/material boundary limit",
                        limits.model_groups
                    );
                }
            }
            b"mtllib" => {
                let libraries_on_line = tokens.count();
                mtl_library_count = mtl_library_count
                    .checked_add(libraries_on_line)
                    .context("OBJ material-library count overflow")?;
                if mtl_library_count > MAX_OBJ_MTL_LIBRARIES {
                    bail!("OBJ exceeds the {MAX_OBJ_MTL_LIBRARIES} material-library safety limit");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn preflight_mtl_reader<R: BufRead>(
    reader: &mut R,
    source_limit: u64,
    line_limit: usize,
) -> Result<MtlPreflightSummary> {
    let mut line = Vec::new();
    let mut bytes_read = 0;
    let mut material_count = 0usize;
    let mut unknown_param_count = 0usize;
    let mut unknown_param_bytes = 0usize;
    let mut material_unknown_keys = std::collections::HashSet::<Vec<u8>>::new();
    while read_bounded_text_line(reader, &mut line, line_limit, source_limit, &mut bytes_read)? {
        let content = line.split(|byte| *byte == b'#').next().unwrap_or_default();
        let mut tokens = content
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|token| !token.is_empty());
        let keyword = tokens.next().unwrap_or_default();
        let values: Vec<_> = tokens.collect();
        validate_mtl_record(keyword, &values)?;
        if keyword == b"newmtl" {
            material_count = material_count
                .checked_add(1)
                .context("MTL material count overflow")?;
            if material_count > MAX_MTL_MATERIALS {
                bail!("MTL exceeds Kalium's {MAX_MTL_MATERIALS} material-slot limit");
            }
            material_unknown_keys.clear();
        } else if !keyword.is_empty() && !is_known_mtl_keyword(keyword) {
            let value_bytes = content.len().saturating_sub(keyword.len());
            if keyword.len() > MAX_MTL_UNKNOWN_KEY_BYTES {
                bail!("MTL unknown parameter name exceeds the safety limit");
            }
            if value_bytes > MAX_MTL_UNKNOWN_VALUE_BYTES {
                bail!("MTL unknown parameter value exceeds the safety limit");
            }
            if material_unknown_keys.insert(keyword.to_vec()) {
                unknown_param_count = unknown_param_count
                    .checked_add(1)
                    .context("MTL unknown parameter count overflow")?;
                if material_unknown_keys.len() > MAX_MTL_UNKNOWN_PARAMS_PER_MATERIAL
                    || unknown_param_count > MAX_MTL_UNKNOWN_PARAMS_TOTAL
                {
                    bail!("MTL exceeds the unknown-parameter safety limit");
                }
                unknown_param_bytes = unknown_param_bytes
                    .checked_add(keyword.len())
                    .and_then(|bytes| bytes.checked_add(MAX_MTL_UNKNOWN_VALUE_BYTES))
                    .context("MTL unknown parameter byte count overflow")?;
                if unknown_param_bytes > MAX_MTL_UNKNOWN_BYTES_TOTAL {
                    bail!("MTL exceeds the unknown-parameter byte safety limit");
                }
            }
        }
    }
    Ok(MtlPreflightSummary {
        material_count,
        unknown_param_count,
        unknown_param_bytes,
    })
}

/// Reject malformed fields that make tobj 4.0.5 panic while parsing MTL
/// records (notably a short Ka/Kd/Ks/Ke vector, whose parser unwraps a failed
/// Vec-to-array conversion). This intentionally validates only the fields
/// consumed by that parser and leaves unknown fields forward-compatible.
fn validate_mtl_record(keyword: &[u8], values: &[&[u8]]) -> Result<()> {
    let parse_float = |value: &[u8]| {
        std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse::<f32>().ok())
            .is_some()
    };
    let malformed = || anyhow::anyhow!("Malformed MTL {keyword:?} record");
    match keyword {
        b"Ka" | b"Kd" | b"Ks" | b"Ke" => {
            if values.len() < 3 || values[..3].iter().any(|value| !parse_float(value)) {
                return Err(malformed());
            }
        }
        b"Ns" | b"Ni" | b"d" => {
            if !values.first().is_some_and(|value| parse_float(value)) {
                return Err(malformed());
            }
        }
        b"illum" => {
            if !values.first().is_some_and(|value| {
                std::str::from_utf8(value)
                    .ok()
                    .and_then(|s| s.parse::<u8>().ok())
                    .is_some()
            }) {
                return Err(malformed());
            }
        }
        b"map_Ka" | b"map_Kd" | b"map_Ks" | b"map_Bump" | b"map_bump" | b"map_Ns" | b"map_ns"
        | b"map_NS" | b"bump" | b"map_d"
            if values.is_empty() =>
        {
            return Err(malformed())
        }
        _ => {}
    }
    Ok(())
}

fn combine_mtl_preflight_summaries(
    total: MtlPreflightSummary,
    next: MtlPreflightSummary,
) -> Result<MtlPreflightSummary> {
    let combined = MtlPreflightSummary {
        material_count: total
            .material_count
            .checked_add(next.material_count)
            .context("MTL aggregate material count overflow")?,
        unknown_param_count: total
            .unknown_param_count
            .checked_add(next.unknown_param_count)
            .context("MTL aggregate unknown parameter count overflow")?,
        unknown_param_bytes: total
            .unknown_param_bytes
            .checked_add(next.unknown_param_bytes)
            .context("MTL aggregate unknown parameter byte count overflow")?,
    };
    if combined.material_count > MAX_MTL_MATERIALS {
        bail!("MTL libraries exceed the aggregate material-slot safety limit");
    }
    if combined.unknown_param_count > MAX_MTL_UNKNOWN_PARAMS_TOTAL
        || combined.unknown_param_bytes > MAX_MTL_UNKNOWN_BYTES_TOTAL
    {
        bail!("MTL libraries exceed the aggregate unknown-parameter safety limit");
    }
    Ok(combined)
}

fn is_known_mtl_keyword(keyword: &[u8]) -> bool {
    matches!(
        keyword,
        b"Ka"
            | b"Kd"
            | b"Ks"
            | b"Ke"
            | b"Ns"
            | b"Ni"
            | b"d"
            | b"illum"
            | b"map_Ka"
            | b"map_Kd"
            | b"map_Ks"
            | b"map_Bump"
            | b"map_bump"
            | b"map_Ns"
            | b"map_ns"
            | b"map_NS"
            | b"bump"
            | b"map_d"
    )
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
    validate_vertex_count(vertices)?;
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

fn validate_vertex_count(vertices: usize) -> Result<()> {
    if vertices > MAX_IMPORTED_VERTICES {
        bail!("Imported mesh exceeds the {MAX_IMPORTED_VERTICES} vertex safety limit");
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

fn check_gltf_material_count(material_count: usize) -> Result<()> {
    if material_count > MAX_GLTF_MATERIALS {
        bail!("glTF exceeds Kalium's {MAX_GLTF_MATERIALS} material-slot safety limit");
    }
    Ok(())
}

fn check_gltf_node_count(node_count: usize) -> Result<()> {
    if node_count > MAX_GLTF_NODES {
        bail!("glTF exceeds the {MAX_GLTF_NODES}-node document limit");
    }
    Ok(())
}

fn check_gltf_node_visit(visits: &mut usize) -> Result<()> {
    *visits = visits
        .checked_add(1)
        .context("glTF node-visit count overflow")?;
    if *visits > MAX_GLTF_NODE_VISITS {
        bail!("glTF scene traversal exceeds the {MAX_GLTF_NODE_VISITS}-node visit limit");
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

    fn small_obj_limits() -> ObjPreflightLimits {
        ObjPreflightLimits {
            source_bytes: 1024,
            line_bytes: 128,
            vertices: 3,
            face_corners: 4,
            indices: 6,
            model_groups: 2,
        }
    }

    #[test]
    fn obj_preflight_accepts_exact_vertex_corner_and_index_limits() {
        let input = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvn 0 0 1\nf 1 1 1 1\n";
        preflight_obj_reader(&mut std::io::Cursor::new(input), small_obj_limits()).unwrap();
    }

    #[test]
    fn obj_preflight_accounts_for_degenerate_short_face_expansion() {
        let limits = small_obj_limits();
        preflight_obj_reader(&mut std::io::Cursor::new(b"f 1 1\nf 1 1\n"), limits).unwrap();

        let error = preflight_obj_reader(&mut std::io::Cursor::new(b"f 1\nf 1\nf 1\n"), limits)
            .unwrap_err();
        assert!(error.to_string().contains("triangulated index"));

        let error =
            preflight_obj_reader(&mut std::io::Cursor::new(b"l 1 2 3 4 5\n"), limits).unwrap_err();
        assert!(error.to_string().contains("face-corner"));
    }

    #[test]
    fn obj_preflight_rejects_oversized_source_attributes_and_face_expansion() {
        let limits = small_obj_limits();
        for input in [
            &b"v 0 0 0\nv 1 0 0\nv 0 1 0\nv 2 2 2\n"[..],
            &b"vt 0 0\nvt 1 0\nvt 0 1\nvt 2 2\n"[..],
            &b"vn 0 0 1\nvn 0 1 0\nvn 1 0 0\nvn 1 1 1\n"[..],
            &b"f 1 1 1 1 1\n"[..],
        ] {
            assert!(preflight_obj_reader(&mut std::io::Cursor::new(input), limits).is_err());
        }

        let mut index_limited = limits;
        index_limited.indices = 5;
        let error = preflight_obj_reader(&mut std::io::Cursor::new(b"f 1 1 1 1\n"), index_limited)
            .unwrap_err();
        assert!(error.to_string().contains("triangulated index"));

        let line_error =
            preflight_obj_reader(&mut std::io::Cursor::new(b"l 1 1 1 1 1\n"), limits).unwrap_err();
        assert!(line_error.to_string().contains("face-corner"));
    }

    #[test]
    fn obj_preflight_bounds_line_size_and_model_boundaries() {
        let mut limits = small_obj_limits();
        limits.line_bytes = 8;
        assert!(
            preflight_obj_reader(&mut std::io::Cursor::new(b"# 12345678\n"), limits)
                .unwrap_err()
                .to_string()
                .contains("line")
        );

        limits = small_obj_limits();
        assert!(preflight_obj_reader(
            &mut std::io::Cursor::new(b"o one\ng two\nusemtl three\n"),
            limits
        )
        .unwrap_err()
        .to_string()
        .contains("boundary"));

        limits = small_obj_limits();
        limits.line_bytes = 4096;
        limits.source_bytes = 4096;
        let make_libraries = |count: usize| {
            let mut input = b"mtllib".to_vec();
            for index in 0..count {
                input.extend_from_slice(format!(" lib{index}.mtl").as_bytes());
            }
            input.push(b'\n');
            input
        };
        preflight_obj_reader(
            &mut std::io::Cursor::new(make_libraries(MAX_OBJ_MTL_LIBRARIES)),
            limits,
        )
        .unwrap();
        assert!(preflight_obj_reader(
            &mut std::io::Cursor::new(make_libraries(MAX_OBJ_MTL_LIBRARIES + 1)),
            limits
        )
        .unwrap_err()
        .to_string()
        .contains("material-library"));
    }

    #[test]
    fn mtl_preflight_rejects_material_count_before_parser_expansion() {
        let mut accepted = String::new();
        for index in 0..MAX_MTL_MATERIALS {
            accepted.push_str(&format!("newmtl material-{index}\n"));
        }
        assert_eq!(
            preflight_mtl_reader(
                &mut std::io::Cursor::new(accepted.into_bytes()),
                MAX_OBJ_SOURCE_BYTES,
                MAX_OBJ_LINE_BYTES,
            )
            .unwrap(),
            MtlPreflightSummary {
                material_count: MAX_MTL_MATERIALS,
                ..MtlPreflightSummary::default()
            }
        );

        let mut source = String::new();
        for index in 0..=MAX_MTL_MATERIALS {
            source.push_str(&format!("newmtl material-{index}\n"));
        }
        assert!(preflight_mtl_reader(
            &mut std::io::Cursor::new(source.into_bytes()),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .unwrap_err()
        .to_string()
        .contains("material-slot limit"));
    }

    #[test]
    fn mtl_preflight_bounds_unknown_parameter_maps_and_values() {
        let mut source = String::from("newmtl bounded\n");
        for index in 0..=MAX_MTL_UNKNOWN_PARAMS_PER_MATERIAL {
            source.push_str(&format!("custom_{index} value\n"));
        }
        let error = preflight_mtl_reader(
            &mut std::io::Cursor::new(source.into_bytes()),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown-parameter"));

        let mut before_first_material = String::new();
        for index in 0..=MAX_MTL_UNKNOWN_PARAMS_PER_MATERIAL {
            before_first_material.push_str(&format!("custom_{index} value\n"));
        }
        assert!(preflight_mtl_reader(
            &mut std::io::Cursor::new(before_first_material.into_bytes()),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .is_err());

        let long_value = format!(
            "newmtl bounded\ncustom {}\n",
            "x".repeat(MAX_MTL_UNKNOWN_VALUE_BYTES)
        );
        assert!(preflight_mtl_reader(
            &mut std::io::Cursor::new(long_value.into_bytes()),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .unwrap_err()
        .to_string()
        .contains("parameter value"));

        let one_unknown = preflight_mtl_reader(
            &mut std::io::Cursor::new(b"newmtl bounded\ncustom value\n"),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .unwrap();
        assert_eq!(one_unknown.unknown_param_count, 1);
        assert_eq!(
            one_unknown.unknown_param_bytes,
            b"custom".len() + MAX_MTL_UNKNOWN_VALUE_BYTES
        );
    }

    #[test]
    fn mtl_library_summaries_enforce_aggregate_material_and_unknown_caps() {
        let half = MtlPreflightSummary {
            material_count: MAX_MTL_MATERIALS / 2,
            unknown_param_count: MAX_MTL_UNKNOWN_PARAMS_TOTAL / 2,
            unknown_param_bytes: MAX_MTL_UNKNOWN_BYTES_TOTAL / 2,
        };
        let total = combine_mtl_preflight_summaries(MtlPreflightSummary::default(), half).unwrap();
        assert_eq!(
            combine_mtl_preflight_summaries(total, half)
                .unwrap()
                .material_count,
            MAX_MTL_MATERIALS
        );

        let too_many_unknowns = MtlPreflightSummary {
            unknown_param_count: 1,
            unknown_param_bytes: MAX_MTL_UNKNOWN_VALUE_BYTES + 1,
            ..MtlPreflightSummary::default()
        };
        let mut at_unknown_cap = total;
        at_unknown_cap.unknown_param_count = MAX_MTL_UNKNOWN_PARAMS_TOTAL;
        assert!(
            combine_mtl_preflight_summaries(at_unknown_cap, too_many_unknowns)
                .unwrap_err()
                .to_string()
                .contains("unknown-parameter")
        );

        let too_many_materials = MtlPreflightSummary {
            material_count: 1,
            ..MtlPreflightSummary::default()
        };
        let mut at_material_cap = total;
        at_material_cap.material_count = MAX_MTL_MATERIALS;
        assert!(
            combine_mtl_preflight_summaries(at_material_cap, too_many_materials)
                .unwrap_err()
                .to_string()
                .contains("material-slot")
        );
    }

    #[test]
    fn obj_import_rejects_repeated_face_amplification_before_tobj_parse() {
        let path =
            std::env::temp_dir().join(format!("kalium-{}-oversized-face.obj", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        writeln!(file, "v 0 0 0\nvt 0 0").unwrap();
        write!(file, "f").unwrap();
        for _ in 0..=MAX_TRIANGLES + 2 {
            write!(file, " 1").unwrap();
        }
        writeln!(file).unwrap();
        drop(file);

        let error = Mesh::from_obj(&path, true).unwrap_err();
        std::fs::remove_file(path).ok();
        assert!(error
            .to_string()
            .contains("triangulated index safety limit"));
    }

    #[test]
    fn obj_import_rejects_many_short_face_expansions_before_tobj_parse() {
        let path = std::env::temp_dir().join(format!(
            "kalium-{}-oversized-short-faces.obj",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        writeln!(file, "v 0 0 0\nvt 0 0").unwrap();
        for _ in 0..=MAX_TRIANGLES {
            writeln!(file, "f 1").unwrap();
        }
        drop(file);

        let error = Mesh::from_obj(&path, true).unwrap_err();
        std::fs::remove_file(path).ok();
        assert!(error
            .to_string()
            .contains("triangulated index safety limit"));
    }

    fn seam_mesh() -> Mesh {
        let vertex = |position, uv| Vertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv,
            tangent: None,
            extensions: JsonMap::new(),
        };
        Mesh {
            name: "seam".into(),
            vertices: vec![
                vertex([0.0, 0.0, 0.0], [0.7, 0.2]),
                vertex([1.0, 0.0, 0.0], [0.9, 0.1]),
                vertex([0.0, 1.0, 0.0], [0.9, 0.9]),
                vertex([1.0, 0.0, 0.0], [0.1, 0.1]),
                vertex([1.0, 1.0, 0.0], [0.3, 0.2]),
                vertex([0.0, 1.0, 0.0], [0.1, 0.9]),
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
            materials: vec!["surface".into()],
            triangle_materials: vec![0, 0],
            extra_uv_sets: Default::default(),
            extensions: JsonMap::new(),
        }
    }

    #[test]
    fn shared_triangle_edge_matches_duplicated_vertices_at_uv_seam() {
        let mesh = seam_mesh();
        let edge = mesh.shared_triangle_edge(0, 1).unwrap();
        assert_eq!(edge.positions, [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        assert_eq!(edge.first_uv, [[0.9, 0.1], [0.9, 0.9]]);
        assert_eq!(edge.second_uv, [[0.1, 0.1], [0.1, 0.9]]);
        assert_eq!(
            mesh.shared_triangle_edge(1, 0).unwrap().positions,
            edge.positions
        );

        let mut disconnected = seam_mesh();
        disconnected.vertices[3].position[0] = 1.001;
        disconnected.vertices[5].position[1] = 1.001;
        assert!(disconnected.shared_triangle_edge(0, 1).is_none());
    }

    #[test]
    fn shared_triangle_edge_matches_near_equal_vertices_at_uv_seam() {
        let mut mesh = seam_mesh();
        mesh.vertices[3].position[0] += 0.0001;
        mesh.vertices[5].position[1] -= 0.0001;

        let edge = mesh.shared_triangle_edge(0, 1).unwrap();
        assert_eq!(edge.positions, [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
        assert_eq!(edge.first_uv, [[0.9, 0.1], [0.9, 0.9]]);
        assert_eq!(edge.second_uv, [[0.1, 0.1], [0.1, 0.9]]);
    }

    #[test]
    fn shared_triangle_edge_rejects_ambiguous_near_coincident_vertices() {
        let mut mesh = seam_mesh();
        // Both second-triangle corners are within tolerance of the first
        // triangle's [1, 0, 0] corner, so choosing either would be arbitrary.
        mesh.vertices[4].position = [1.00005, 0.0, 0.0];
        assert!(mesh.shared_triangle_edge(0, 1).is_none());
    }

    #[test]
    fn shared_triangle_edge_rejects_coincident_ambiguous_triangles() {
        let mut mesh = seam_mesh();
        mesh.vertices[3].position = mesh.vertices[0].position;
        mesh.vertices[4].position = mesh.vertices[1].position;
        mesh.vertices[5].position = mesh.vertices[2].position;
        assert!(mesh.shared_triangle_edge(0, 1).is_none());
    }

    fn write_gltf_triangle(path: &Path, scale: [f32; 3], include_normals: bool) {
        let mut bytes = Vec::new();
        let positions = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        for value in positions {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let position_bytes = bytes.len();
        let texcoords = [0.25f32, 0.75, 0.25, 0.75, 0.25, 0.75];
        for value in texcoords {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let texcoord_bytes = bytes.len() - position_bytes;
        if include_normals {
            for value in [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }

        let mut buffer_views = vec![
            serde_json::json!({"buffer": 0, "byteOffset": 0, "byteLength": position_bytes}),
            serde_json::json!({"buffer": 0, "byteOffset": position_bytes, "byteLength": texcoord_bytes}),
        ];
        let mut accessors = vec![
            serde_json::json!({"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]}),
            serde_json::json!({"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC2"}),
        ];
        let mut attributes = serde_json::json!({"POSITION": 0, "TEXCOORD_0": 1});
        if include_normals {
            buffer_views.push(serde_json::json!({"buffer": 0, "byteOffset": position_bytes + texcoord_bytes, "byteLength": 36}));
            accessors.push(serde_json::json!({"bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC3"}));
            attributes["NORMAL"] = serde_json::json!(2);
        }

        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "scale": scale}],
            "meshes": [{"primitives": [{"attributes": attributes, "material": 1}]}],
            "materials": [{"name": "Unused"}, {"name": "Reflected"}],
            "buffers": [{
                "byteLength": bytes.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&bytes))
            }],
            "bufferViews": buffer_views,
            "accessors": accessors
        });
        std::fs::write(path, serde_json::to_vec(&gltf).unwrap()).unwrap();
    }

    fn write_gltf_tangent_triangle(
        path: &Path,
        scale: [f32; 3],
        normals: [[f32; 3]; 3],
        tangents: &[[f32; 4]],
    ) {
        let mut bytes = Vec::new();
        let mut buffer_views = Vec::new();
        let mut accessors = Vec::new();
        let mut attributes = serde_json::Map::new();
        let positions = [[0.0f32, 0.0, 0.0], [1.0f32, 0.0, 0.0], [0.0f32, 1.0, 0.0]];
        let texcoords = [[0.0f32, 0.0], [1.0, 0.0], [0.0, 1.0]];
        for (semantic, values, component_count, accessor_type) in [
            ("POSITION", positions.as_slice(), 3usize, "VEC3"),
            ("NORMAL", normals.as_slice(), 3usize, "VEC3"),
        ] {
            let offset = bytes.len();
            for value in values.iter().flat_map(|value| value.iter()) {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            let view_index = buffer_views.len();
            buffer_views.push(serde_json::json!({
                "buffer": 0,
                "byteOffset": offset,
                "byteLength": values.len() * component_count * 4
            }));
            let accessor_index = accessors.len();
            let mut accessor = serde_json::json!({
                "bufferView": view_index,
                "componentType": 5126,
                "count": values.len(),
                "type": accessor_type
            });
            if semantic == "POSITION" {
                accessor["min"] = serde_json::json!([0.0, 0.0, 0.0]);
                accessor["max"] = serde_json::json!([1.0, 1.0, 0.0]);
            }
            accessors.push(accessor);
            attributes.insert(semantic.to_string(), serde_json::json!(accessor_index));
        }
        let offset = bytes.len();
        for value in texcoords.iter().flat_map(|value| value.iter()) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let texcoord_view = buffer_views.len();
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": texcoords.len() * 2 * 4
        }));
        let texcoord_accessor = accessors.len();
        accessors.push(serde_json::json!({
            "bufferView": texcoord_view,
            "componentType": 5126,
            "count": texcoords.len(),
            "type": "VEC2"
        }));
        attributes.insert("TEXCOORD_0".into(), serde_json::json!(texcoord_accessor));

        let offset = bytes.len();
        for value in tangents.iter().flat_map(|value| value.iter()) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let tangent_view = buffer_views.len();
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": tangents.len() * 4 * 4
        }));
        let tangent_accessor = accessors.len();
        accessors.push(serde_json::json!({
            "bufferView": tangent_view,
            "componentType": 5126,
            "count": tangents.len(),
            "type": "VEC4"
        }));
        attributes.insert("TANGENT".into(), serde_json::json!(tangent_accessor));
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "scale": scale}],
            "meshes": [{"primitives": [{"attributes": attributes}]}],
            "buffers": [{
                "byteLength": bytes.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&bytes))
            }],
            "bufferViews": buffer_views,
            "accessors": accessors
        });
        std::fs::write(path, serde_json::to_vec(&gltf).unwrap()).unwrap();
    }

    fn mutation_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kalium-parser-mutations-{}-{name}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assert_import_does_not_panic(path: &Path) -> Result<()> {
        let result = std::panic::catch_unwind(|| Mesh::from_file(path));
        let imported = result
            .unwrap_or_else(|_| panic!("mutated mesh parser must not panic: {}", path.display()))?;
        imported.validate()?;
        Ok(())
    }

    fn write_glb_triangle(path: &Path) {
        let gltf_path = path.with_extension("seed.gltf");
        write_gltf_triangle(&gltf_path, [1.0, 1.0, 1.0], true);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&gltf_path).unwrap()).unwrap();
        let uri = json["buffers"][0]["uri"].as_str().unwrap();
        let encoded = uri.split_once(',').unwrap().1;
        let binary = base64::decode(encoded).unwrap();
        json["buffers"][0].as_object_mut().unwrap().remove("uri");
        let mut json_bytes = serde_json::to_vec(&json).unwrap();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let mut binary_bytes = binary;
        while !binary_bytes.len().is_multiple_of(4) {
            binary_bytes.push(0);
        }
        let total_len = 12 + 8 + json_bytes.len() + 8 + binary_bytes.len();
        let mut glb = Vec::with_capacity(total_len);
        glb.extend_from_slice(&0x4654_6c67u32.to_le_bytes());
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total_len as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x4e4f_534au32.to_le_bytes());
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(binary_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x004e_4942u32.to_le_bytes());
        glb.extend_from_slice(&binary_bytes);
        std::fs::write(path, glb).unwrap();
        std::fs::remove_file(gltf_path).ok();
    }

    #[test]
    fn obj_and_mtl_seed_truncations_and_byte_mutations_never_panic() {
        let dir = mutation_test_dir("obj-mtl");
        let obj_path = dir.join("seed.obj");
        let mtl_path = dir.join("seed.mtl");
        let obj_seed = b"mtllib seed.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nusemtl Test\nf 1/1 2/2 3/3\n";
        let mtl_seed = b"newmtl Test\nKd 0.7 0.5 0.3\ncustom_note preserved\n";
        assert!(preflight_mtl_reader(
            &mut std::io::Cursor::new(b"newmtl Test\nKd 0.7\n"),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .unwrap_err()
        .to_string()
        .contains("Malformed MTL"));

        // Exercise every prefix, including the empty document and cuts inside
        // tokens, for both text parsers. Invalid MTL files may be ignored by
        // tobj, but must never escape as a panic or produce an invalid Mesh.
        for prefix in 0..=obj_seed.len() {
            std::fs::write(&obj_path, &obj_seed[..prefix]).unwrap();
            std::fs::write(&mtl_path, mtl_seed).unwrap();
            let _ = assert_import_does_not_panic(&obj_path);
        }
        std::fs::write(&obj_path, obj_seed).unwrap();
        for prefix in 0..=mtl_seed.len() {
            std::fs::write(&mtl_path, &mtl_seed[..prefix]).unwrap();
            let _ = assert_import_does_not_panic(&obj_path);
        }

        // A fixed stride and replacement alphabet gives repeatable malformed
        // token, index, delimiter, and numeric-field mutations without an
        // unbounded fuzz loop.
        for (path, seed) in [(&obj_path, &obj_seed[..]), (&mtl_path, &mtl_seed[..])] {
            for offset in (0..seed.len()).step_by(3) {
                for replacement in *b"!\x009" {
                    let mut mutated = seed.to_vec();
                    mutated[offset] = replacement;
                    std::fs::write(path, mutated).unwrap();
                    let _ = assert_import_does_not_panic(&obj_path);
                }
            }
        }

        // Amplify only bounded, small records. The preflight cap must reject
        // before the downstream OBJ/MTL parser expands these collections.
        let mut amplified_obj = b"v 0 0 0\n".to_vec();
        for _ in 0..=MAX_OBJ_MODEL_GROUPS {
            amplified_obj.extend_from_slice(b"g x\n");
        }
        std::fs::write(&obj_path, amplified_obj).unwrap();
        let group_error = std::panic::catch_unwind(|| Mesh::from_file(&obj_path))
            .expect("amplified OBJ parser must not panic")
            .unwrap_err()
            .to_string();
        assert!(group_error.contains("object/group/material boundary limit"));

        let mut amplified_mtl = Vec::new();
        for _ in 0..=MAX_MTL_MATERIALS {
            amplified_mtl.extend_from_slice(b"newmtl x\n");
        }
        assert!(preflight_mtl_reader(
            &mut std::io::Cursor::new(amplified_mtl.clone()),
            MAX_OBJ_SOURCE_BYTES,
            MAX_OBJ_LINE_BYTES,
        )
        .unwrap_err()
        .to_string()
        .contains("material-slot limit"));
        std::fs::write(&mtl_path, amplified_mtl).unwrap();
        std::fs::write(
            &obj_path,
            b"mtllib seed.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nusemtl x\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let material_error = std::panic::catch_unwind(|| Mesh::from_file(&obj_path))
            .expect("amplified MTL parser must not panic")
            .unwrap_err()
            .to_string();
        assert!(
            material_error.contains("material-slot limit"),
            "{material_error}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_and_glb_seed_truncations_and_byte_mutations_never_panic() {
        let dir = mutation_test_dir("gltf-glb");
        let path = dir.join("seed.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let seed = std::fs::read(&path).unwrap();
        assert_import_does_not_panic(&path).unwrap();
        for prefix in 0..=seed.len() {
            std::fs::write(&path, &seed[..prefix]).unwrap();
            let _ = assert_import_does_not_panic(&path);
        }
        for offset in (0..seed.len()).step_by(17) {
            for replacement in [0, b'!', b'9'] {
                let mut mutated = seed.clone();
                mutated[offset] = replacement;
                std::fs::write(&path, mutated).unwrap();
                let _ = assert_import_does_not_panic(&path);
            }
        }

        let glb_path = dir.join("seed.glb");
        write_glb_triangle(&glb_path);
        let seed = std::fs::read(&glb_path).unwrap();
        assert_import_does_not_panic(&glb_path).unwrap();
        for prefix in 0..=seed.len() {
            std::fs::write(&glb_path, &seed[..prefix]).unwrap();
            let _ = assert_import_does_not_panic(&glb_path);
        }
        for offset in (0..seed.len()).step_by(7) {
            for replacement in [0, b'!', b'9'] {
                let mut mutated = seed.clone();
                mutated[offset] = replacement;
                std::fs::write(&glb_path, mutated).unwrap();
                let _ = assert_import_does_not_panic(&glb_path);
            }
        }

        // Amplified node records stay under a half-megabyte JSON document but
        // exceed the parser's explicit collection cap and must fail cleanly.
        let path = dir.join("amplified.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["scenes"] = serde_json::json!([{"nodes": []}]);
        gltf["nodes"] = serde_json::Value::Array(
            (0..=MAX_GLTF_NODES)
                .map(|_| serde_json::json!({}))
                .collect(),
        );
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let node_error = std::panic::catch_unwind(|| Mesh::from_file(&path))
            .expect("amplified glTF parser must not panic")
            .unwrap_err();
        let node_error = node_error.to_string();
        assert!(node_error.contains("node document limit"));

        // The file size gate is checked before JSON parsing or allocation.
        let over_file_limit = dir.join("oversized.gltf");
        let sparse = File::create(&over_file_limit).unwrap();
        sparse.set_len(MAX_GLTF_FILE_BYTES + 1).unwrap();
        let size_error = std::panic::catch_unwind(|| Mesh::from_file(&over_file_limit))
            .expect("oversized glTF parser must not panic")
            .unwrap_err()
            .to_string();
        assert!(size_error.contains("source exceeds the"));
        std::fs::remove_dir_all(dir).ok();
    }

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
    fn obj_import_can_preserve_coordinates_for_alignment() {
        let dir = std::env::temp_dir().join(format!("kalium-source-space-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj = dir.join("offset.obj");
        std::fs::write(
            &obj,
            "v 10 20 30\nv 14 20 30\nv 10 26 30\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();

        let normalized = Mesh::from_file(&obj).unwrap();
        let preserved = Mesh::from_file_preserving_coordinates(&obj).unwrap();
        assert_eq!(preserved.vertices[0].position, [10.0, 20.0, 30.0]);
        assert_eq!(preserved.vertices[1].position, [14.0, 20.0, 30.0]);
        assert_ne!(
            normalized.vertices[0].position,
            preserved.vertices[0].position
        );
        preserved.validate().unwrap();
        std::fs::remove_dir_all(dir).ok();
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
        assert!(mesh.vertices.iter().all(|vertex| vertex.tangent.is_none()));
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
            "mtllib ../outside.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nusemtl External\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let error = Mesh::from_file(&obj).unwrap_err().to_string();
        assert!(error.contains("outside the asset directory"), "{error}");
        std::fs::remove_dir_all(parent).ok();
    }

    #[test]
    fn obj_import_reports_missing_material_libraries() {
        let dir = mutation_test_dir("missing-mtl");
        let obj = dir.join("missing.mtl.obj");
        std::fs::write(
            &obj,
            b"mtllib absent.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nusemtl Test\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let error = Mesh::from_file(&obj).unwrap_err().to_string();
        assert!(
            error.contains("Could not resolve referenced material library"),
            "{error}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn obj_import_without_material_library_still_succeeds() {
        let dir = mutation_test_dir("no-mtl");
        let obj = dir.join("triangle.obj");
        std::fs::write(
            &obj,
            b"v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let mesh = Mesh::from_file(&obj).unwrap();
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        assert_eq!(mesh.materials, vec!["Default"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn obj_import_reports_malformed_short_mtl_numeric_fields() {
        let dir = mutation_test_dir("malformed-mtl");
        let obj = dir.join("malformed.mtl.obj");
        std::fs::write(dir.join("bad.mtl"), "newmtl Test\nKd 0.7\n").unwrap();
        std::fs::write(
            &obj,
            b"mtllib bad.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nusemtl Test\nf 1/1 2/2 3/3\n",
        )
        .unwrap();
        let error = Mesh::from_file(&obj).unwrap_err().to_string();
        assert!(
            error.contains("Invalid referenced material library"),
            "{error}"
        );
        assert!(error.contains("Malformed MTL"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn obj_without_complete_uvs_is_rejected_with_actionable_error() {
        let dir =
            std::env::temp_dir().join(format!("kalium-obj-missing-uv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("triangle.obj");
        std::fs::write(&path, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap();
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("texture coordinates (vt)"), "{error}");
        assert!(error.contains("add UVs before importing"), "{error}");
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
    fn mesh_validation_rejects_invalid_persisted_tangent_frames() {
        let mut mesh = Mesh::demo_cube();
        mesh.vertices[0].tangent = Some([1.0, 0.0, 0.0, 1.0]);
        mesh.validate().unwrap();

        mesh.vertices[0].tangent = Some([2.0, 0.0, 0.0, 1.0]);
        assert!(mesh.validate().is_err());

        mesh.vertices[0].tangent = Some([1.0, 0.0, 0.0, 0.0]);
        assert!(mesh.validate().is_err());

        mesh.vertices[0].tangent = Some([0.0, 0.0, 1.0, 1.0]);
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
        assert!(validate_vertex_count(MAX_IMPORTED_VERTICES).is_ok());
        assert!(validate_vertex_count(MAX_IMPORTED_VERTICES + 1)
            .unwrap_err()
            .to_string()
            .contains("vertex safety limit"));
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
    fn gltf_document_node_count_is_bounded_even_for_unreachable_nodes() {
        assert!(check_gltf_node_count(MAX_GLTF_NODES).is_ok());
        assert!(check_gltf_node_count(MAX_GLTF_NODES + 1)
            .unwrap_err()
            .to_string()
            .contains("document limit"));

        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-unreachable-node-cap-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unreachable-nodes.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["scenes"] = serde_json::json!([{"nodes": []}]);
        gltf["nodes"] = serde_json::Value::Array(
            (0..=MAX_GLTF_NODES)
                .map(|_| serde_json::json!({}))
                .collect(),
        );
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("100000-node document limit"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_reader_enforces_the_byte_limit_at_the_boundary() {
        let json = br#"{"asset":{"version":"2.0"}}"#;
        assert!(parse_gltf_reader(std::io::Cursor::new(&json[..]), json.len() as u64).is_ok());

        let mut oversized = json.to_vec();
        oversized.push(b' ');
        let error = parse_gltf_reader(std::io::Cursor::new(&oversized[..]), json.len() as u64)
            .unwrap_err()
            .to_string();
        assert!(error.contains("source exceeds the"), "{error}");
    }

    #[test]
    fn gltf_node_preflight_enforces_limit_for_json_and_glb() {
        fn node_document(node_count: usize) -> Vec<u8> {
            let mut json = String::from(r#"{"asset":{"version":"2.0"},"nodes":["#);
            for index in 0..node_count {
                if index > 0 {
                    json.push(',');
                }
                json.push_str("{}");
            }
            json.push_str("]}");
            json.into_bytes()
        }

        fn glb_with_json(mut json: Vec<u8>) -> Vec<u8> {
            while !json.len().is_multiple_of(4) {
                json.push(b' ');
            }
            let total_len = 20 + json.len();
            let mut glb = Vec::with_capacity(total_len);
            glb.extend_from_slice(&0x4654_6c67u32.to_le_bytes());
            glb.extend_from_slice(&2u32.to_le_bytes());
            glb.extend_from_slice(&(total_len as u32).to_le_bytes());
            glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
            glb.extend_from_slice(&0x4e4f_534au32.to_le_bytes());
            glb.extend_from_slice(&json);
            glb
        }

        let json_at_limit = node_document(MAX_GLTF_NODES);
        assert!(preflight_gltf_node_count(
            &mut std::io::Cursor::new(&json_at_limit),
            MAX_GLTF_FILE_BYTES
        )
        .is_ok());
        let json_over_limit = node_document(MAX_GLTF_NODES + 1);
        let error = preflight_gltf_node_count(
            &mut std::io::Cursor::new(&json_over_limit),
            MAX_GLTF_FILE_BYTES,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("100000-node document limit"), "{error}");

        let glb_at_limit = glb_with_json(json_at_limit);
        assert!(preflight_gltf_node_count(
            &mut std::io::Cursor::new(&glb_at_limit),
            MAX_GLTF_FILE_BYTES
        )
        .is_ok());
        let glb_over_limit = glb_with_json(json_over_limit);
        let error = preflight_gltf_node_count(
            &mut std::io::Cursor::new(&glb_over_limit),
            MAX_GLTF_FILE_BYTES,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("100000-node document limit"), "{error}");
    }

    #[test]
    fn gltf_material_slots_are_capped_before_material_string_assembly() {
        assert!(check_gltf_material_count(MAX_GLTF_MATERIALS).is_ok());
        assert!(check_gltf_material_count(MAX_GLTF_MATERIALS + 1)
            .unwrap_err()
            .to_string()
            .contains("material-slot safety limit"));

        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-material-cap-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("materials.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["materials"] = serde_json::Value::Array(
            (0..MAX_GLTF_MATERIALS)
                .map(|_| serde_json::json!({}))
                .collect(),
        );
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let mesh = Mesh::from_file(&path).unwrap();
        assert_eq!(mesh.materials.len(), MAX_GLTF_MATERIALS);
        assert_eq!(mesh.materials[0], "Material 1");
        assert_eq!(mesh.materials[MAX_GLTF_MATERIALS - 1], "Material 256");

        gltf["materials"] = serde_json::Value::Array(
            (0..=MAX_GLTF_MATERIALS)
                .map(|_| serde_json::json!({}))
                .collect(),
        );
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("material-slot safety limit"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_node_visit_budget_bounds_repeated_child_references() {
        let mut visits = 0;
        for _ in 0..MAX_GLTF_NODE_VISITS {
            check_gltf_node_visit(&mut visits).unwrap();
        }
        let error = check_gltf_node_visit(&mut visits).unwrap_err().to_string();
        assert!(error.contains("100000-node visit limit"), "{error}");

        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-node-visit-cap-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("repeated-children.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["scenes"] = serde_json::json!([{"nodes": [0]}]);
        gltf["nodes"] = serde_json::json!([
            {"children": vec![1; MAX_GLTF_NODE_VISITS]},
            {}
        ]);
        gltf["scene"] = serde_json::json!(0);
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("100000-node visit limit"), "{error}");
        std::fs::remove_dir_all(dir).ok();
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
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["images"] = serde_json::json!([{"uri": "data:image/png;base64,not-an-image"}]);
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let mesh = Mesh::from_file(&path).unwrap();
        assert_eq!(mesh.vertices.len(), 3);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_triangle_without_texcoord_zero_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-missing-uv-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("missing-uv.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["meshes"][0]["primitives"][0]["attributes"]
            .as_object_mut()
            .unwrap()
            .remove("TEXCOORD_0");
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("mesh 0 primitive 0"), "{error}");
        assert!(error.contains("TEXCOORD_0"), "{error}");
        assert!(error.contains("add UVs before importing"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_mixed_uv_primitives_report_the_invalid_primitive() {
        let dir =
            std::env::temp_dir().join(format!("kalium-gltf-mixed-uv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mixed-uv.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let valid = gltf["meshes"][0]["primitives"][0].clone();
        let mut invalid = valid.clone();
        invalid["attributes"]
            .as_object_mut()
            .unwrap()
            .remove("TEXCOORD_0");
        gltf["meshes"][0]["primitives"] = serde_json::json!([valid, invalid]);
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("mesh 0 primitive 1"), "{error}");
        assert!(error.contains("TEXCOORD_0"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_import_rejects_features_that_mesh_storage_cannot_preserve() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-unsupported-fidelity-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unsupported.gltf");

        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let base: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();

        let mut morph = base.clone();
        morph["meshes"][0]["primitives"][0]["targets"] = serde_json::json!([{"POSITION": 0}]);
        std::fs::write(&path, serde_json::to_vec(&morph).unwrap()).unwrap();
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("morph targets are not supported"), "{error}");

        let mut skinned = base;
        skinned["nodes"][0]["skin"] = serde_json::json!(0);
        skinned["nodes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({}));
        skinned["skins"] = serde_json::json!([{"joints": [1]}]);
        std::fs::write(&path, serde_json::to_vec(&skinned).unwrap()).unwrap();
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("skinning is not supported"), "{error}");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_import_preserves_extra_uv_sets_across_primitives_and_mesh_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("kalium-gltf-extra-uv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extra-uv.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let uri = gltf["buffers"][0]["uri"].as_str().unwrap();
        let mut bytes = base64::decode(uri.split_once(',').unwrap().1).unwrap();
        let uv1_values = [0.8f32, 0.1, 0.6, 0.2, 0.4, 0.3];
        let uv1_offset = bytes.len();
        for value in uv1_values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let view_index = gltf["bufferViews"].as_array().unwrap().len();
        gltf["bufferViews"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "buffer": 0,
                "byteOffset": uv1_offset,
                "byteLength": uv1_values.len() * 4
            }));
        let accessor_index = gltf["accessors"].as_array().unwrap().len();
        gltf["accessors"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "bufferView": view_index,
                "componentType": 5126,
                "count": 3,
                "type": "VEC2"
            }));
        let primitive_without_uv1 = gltf["meshes"][0]["primitives"][0].clone();
        let mut primitive_with_uv1 = primitive_without_uv1.clone();
        primitive_with_uv1["attributes"]["TEXCOORD_1"] = serde_json::json!(accessor_index);
        gltf["meshes"][0]["primitives"] =
            serde_json::json!([primitive_with_uv1, primitive_without_uv1]);
        gltf["buffers"][0]["byteLength"] = serde_json::json!(bytes.len());
        gltf["buffers"][0]["uri"] = serde_json::json!(format!(
            "data:application/octet-stream;base64,{}",
            base64::encode(bytes)
        ));
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();

        let mesh = Mesh::from_file(&path).unwrap();
        let uv1 = &mesh.extra_uv_sets[&1];
        assert_eq!(uv1.len(), 6);
        assert_eq!(uv1[0], Some([0.8, 0.1]));
        assert_eq!(uv1[1], Some([0.6, 0.2]));
        assert_eq!(uv1[2], Some([0.4, 0.3]));
        assert_eq!(&uv1[3..], &[None, None, None]);

        let encoded = serde_json::to_vec(&mesh).unwrap();
        let restored: Mesh = serde_json::from_slice(&encoded).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.extra_uv_sets, mesh.extra_uv_sets);
        let mut legacy_value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        legacy_value
            .as_object_mut()
            .unwrap()
            .remove("extra_uv_sets");
        let legacy: Mesh = serde_json::from_value(legacy_value).unwrap();
        assert!(legacy.extra_uv_sets.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_import_rejects_uv_set_indices_beyond_bounded_storage() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-uv-index-limit-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("too-many-uv-sets.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_8"] = serde_json::json!(1);
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("TEXCOORD_0 through TEXCOORD_7"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_import_uses_first_scene_when_default_is_absent_and_honors_declared_default() {
        let dir =
            std::env::temp_dir().join(format!("kalium-gltf-scenes-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scenes.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["scenes"] = serde_json::json!([{"nodes": [0]}, {"nodes": [0, 1]}]);
        gltf["nodes"] = serde_json::json!([{"mesh": 0}, {"mesh": 0}]);

        // An explicit default selects the second scene, which contains two
        // nodes. The shared node appears once despite also belonging to scene 0.
        gltf["scene"] = serde_json::json!(1);
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let explicit_default_mesh = Mesh::from_file(&path).unwrap();
        assert_eq!(explicit_default_mesh.vertices.len(), 6);
        assert_eq!(explicit_default_mesh.indices.len(), 6);

        // Without a default, only the first declared scene is imported; the
        // node shared with the alternative scene is not duplicated.
        gltf.as_object_mut().unwrap().remove("scene");
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        let fallback_mesh = Mesh::from_file(&path).unwrap();
        assert_eq!(fallback_mesh.vertices.len(), 3);
        assert_eq!(fallback_mesh.indices, vec![0, 1, 2]);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_node_translation_rotation_and_nonuniform_scale_are_baked() {
        let dir =
            std::env::temp_dir().join(format!("kalium-gltf-transform-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transformed.gltf");
        write_gltf_triangle(&path, [2.0, 1.0, 1.0], true);
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["nodes"][0]["translation"] = serde_json::json!([2, 3, 0]);
        gltf["nodes"][0]["rotation"] = serde_json::json!([0, 0, 0.70710677, 0.70710677]);
        gltf["nodes"][0]["scale"] = serde_json::json!([2, 1, 1]);
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
        for value in [0.0f32, 1.0, 1.0, 1.0, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let gltf = serde_json::json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "scale": [2, 1, 1]}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}}]}],
            "buffers": [{
                "byteLength": bytes.len(),
                "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&bytes))
            }],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36},
                {"buffer": 0, "byteOffset": 36, "byteLength": 36},
                {"buffer": 0, "byteOffset": 72, "byteLength": 24}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-1, 0, 0], "max": [0, 1, 1]},
                {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"},
                {"bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC2"}
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

    #[test]
    fn gltf_tangents_are_imported_and_transformed_for_nonuniform_reflection() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-tangent-transform-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tangent.gltf");
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        write_gltf_tangent_triangle(
            &path,
            [-2.0, 1.0, 1.0],
            [[diagonal, diagonal, 0.0]; 3],
            &[[diagonal, -diagonal, 0.0, 1.0]; 3],
        );

        let mesh = Mesh::from_file(&path).unwrap();
        for vertex in &mesh.vertices {
            let normal = Vec3::from_array(vertex.normal);
            let tangent = vertex
                .tangent
                .expect("supplied glTF tangent should be retained");
            let tangent_direction = Vec3::new(tangent[0], tangent[1], tangent[2]);
            assert!((normal.length() - 1.0).abs() < 1.0e-6);
            assert!((tangent_direction.length() - 1.0).abs() < 1.0e-6);
            assert!(normal.dot(tangent_direction).abs() < 1.0e-6);
            assert!((normal.x + 1.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
            assert!((normal.y - 2.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
            assert!((tangent[0] + 2.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
            assert!((tangent[1] + 1.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
            assert_eq!(tangent[2], 0.0);
            assert_eq!(tangent[3], -1.0);
        }
        let round_trip: Mesh = serde_json::from_slice(&serde_json::to_vec(&mesh).unwrap()).unwrap();
        assert_eq!(
            round_trip
                .vertices
                .iter()
                .map(|vertex| vertex.tangent)
                .collect::<Vec<_>>(),
            mesh.vertices
                .iter()
                .map(|vertex| vertex.tangent)
                .collect::<Vec<_>>()
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_tangents_without_a_source_attribute_remain_absent() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-no-tangent-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("no-tangent.gltf");
        write_gltf_triangle(&path, [1.0, 1.0, 1.0], true);
        let mesh = Mesh::from_file(&path).unwrap();
        assert!(mesh.vertices.iter().all(|vertex| vertex.tangent.is_none()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_tangents_reject_invalid_values_and_count_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-invalid-tangent-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (label, tangents, expected_error) in [
            (
                "nonfinite",
                vec![[f32::INFINITY, 0.0, 0.0, 1.0]; 3],
                "non-finite, degenerate, or invalid",
            ),
            (
                "degenerate",
                vec![[0.0, 0.0, 0.0, 1.0]; 3],
                "non-finite, degenerate, or invalid",
            ),
            (
                "handedness",
                vec![[1.0, 0.0, 0.0, 0.0]; 3],
                "non-finite, degenerate, or invalid",
            ),
            (
                "count-mismatch",
                vec![[1.0, 0.0, 0.0, 1.0]; 2],
                "count does not match",
            ),
        ] {
            let path = dir.join(format!("{label}.gltf"));
            write_gltf_tangent_triangle(&path, [1.0, 1.0, 1.0], [[0.0, 0.0, 1.0]; 3], &tangents);
            let error = Mesh::from_file(&path).unwrap_err().to_string();
            assert!(error.contains(expected_error), "{label}: {error}");
        }
        let path = dir.join("degenerate-after-projection.gltf");
        write_gltf_tangent_triangle(
            &path,
            [1.0, 1.0, 1.0],
            [[0.0, 0.0, 1.0]; 3],
            &[[0.0, 0.0, 1.0, 1.0]; 3],
        );
        let error = Mesh::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("degenerate after transformation"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_vertex_json_without_tangent_deserializes_as_none() {
        let vertex: Vertex = serde_json::from_value(serde_json::json!({
            "position": [1.0, 2.0, 3.0],
            "normal": [0.0, 0.0, 1.0],
            "uv": [0.25, 0.75]
        }))
        .unwrap();
        assert_eq!(vertex.position, [1.0, 2.0, 3.0]);
        assert_eq!(vertex.tangent, None);
    }

    #[test]
    fn gltf_reflection_reverses_winding_and_preserves_normals_uvs_and_material_picking() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-reflection-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        for (case, include_normals) in [("generated", false), ("supplied", true)] {
            let path = dir.join(format!("{case}.gltf"));
            write_gltf_triangle(&path, [-1.0, 1.0, 1.0], include_normals);
            let mesh = Mesh::from_file(&path).unwrap();

            assert_eq!(mesh.indices, vec![0, 2, 1]);
            assert_eq!(mesh.materials, vec!["Unused", "Reflected"]);
            assert_eq!(mesh.triangle_materials, vec![1]);
            let [a, b, c] = [mesh.indices[0], mesh.indices[1], mesh.indices[2]];
            let edge1 = Vec3::from_array(mesh.vertices[b as usize].position)
                - Vec3::from_array(mesh.vertices[a as usize].position);
            let edge2 = Vec3::from_array(mesh.vertices[c as usize].position)
                - Vec3::from_array(mesh.vertices[a as usize].position);
            let geometric_normal = edge1.cross(edge2).normalize();
            assert!(geometric_normal.z > 0.999);
            for vertex in &mesh.vertices {
                let normal = Vec3::from_array(vertex.normal);
                assert!(normal.is_finite());
                assert!((normal.length() - 1.0).abs() < 1.0e-6);
                assert!(normal.z > 0.999, "unexpected reflected normal: {normal:?}");
            }

            let doc = crate::document::Document::new(64);
            let material = crate::renderer::Material::from_document(&doc);
            let mut viewport = crate::renderer::Viewport::new(200, 200);
            viewport.render(&mesh, &material, None);
            let (x, y) = (0..viewport.height)
                .flat_map(|y| (0..viewport.width).map(move |x| (x, y)))
                .find(|(x, y)| viewport.pick(*x, *y).is_some())
                .expect("reflected triangle should remain pickable");
            let uv = viewport.pick(x, y).unwrap();
            assert!((uv[0] - 0.25).abs() < 1.0e-6);
            assert!((uv[1] - 0.75).abs() < 1.0e-6);
            assert_eq!(viewport.pick_material_slot(x, y), Some(1));
        }

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gltf_singular_node_transform_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "kalium-gltf-singular-transform-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("singular.gltf");
        write_gltf_triangle(&path, [0.0, 1.0, 1.0], true);

        let error = Mesh::from_file(&path).unwrap_err();
        assert!(error.to_string().contains("singular or non-finite"));
        std::fs::remove_dir_all(dir).ok();
    }
}

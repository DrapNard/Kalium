//! Small, bounded procedural graph primitives for deterministic material data.
//!
//! Graphs support UV-driven value noise with an optional stable-ID Occlusion
//! layer input, producing a Roughness output. The module stays independent of
//! the document model; the compositor supplies resolved source pixels.

use anyhow::{bail, Result};
use image::{ImageBuffer, RgbaImage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const MAX_GRAPH_NODES: usize = 64;
pub const MAX_GRAPH_EDGES: usize = 64;
pub const MIN_NOISE_SCALE: f32 = 0.01;
pub const MAX_NOISE_SCALE: f32 = 2048.0;
pub const MAX_GRAPH_OUTPUT_SIZE: u32 = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProceduralGraph {
    pub nodes: Vec<ProceduralNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProceduralNode {
    pub id: u32,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    Uv,
    ValueNoise { seed: u64, scale: f32 },
    OcclusionInput { source_layer_id: u32 },
    Multiply,
    RoughnessOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: u32,
    pub to: u32,
}

impl ProceduralGraph {
    /// A useful default graph: UV → seeded noise → Roughness.
    pub fn value_noise(seed: u64, scale: f32) -> Self {
        Self {
            nodes: vec![
                ProceduralNode {
                    id: 1,
                    kind: NodeKind::Uv,
                },
                ProceduralNode {
                    id: 2,
                    kind: NodeKind::ValueNoise { seed, scale },
                },
                ProceduralNode {
                    id: 3,
                    kind: NodeKind::RoughnessOutput,
                },
            ],
            edges: vec![GraphEdge { from: 1, to: 2 }, GraphEdge { from: 2, to: 3 }],
        }
    }

    /// Build a graph that modulates seeded noise by a same-Texture-Set AO layer.
    pub fn value_noise_times_occlusion(seed: u64, scale: f32, source_layer_id: u32) -> Self {
        Self {
            nodes: vec![
                ProceduralNode {
                    id: 1,
                    kind: NodeKind::Uv,
                },
                ProceduralNode {
                    id: 2,
                    kind: NodeKind::ValueNoise { seed, scale },
                },
                ProceduralNode {
                    id: 3,
                    kind: NodeKind::OcclusionInput { source_layer_id },
                },
                ProceduralNode {
                    id: 4,
                    kind: NodeKind::Multiply,
                },
                ProceduralNode {
                    id: 5,
                    kind: NodeKind::RoughnessOutput,
                },
            ],
            edges: vec![
                GraphEdge { from: 1, to: 2 },
                GraphEdge { from: 2, to: 4 },
                GraphEdge { from: 3, to: 4 },
                GraphEdge { from: 4, to: 5 },
            ],
        }
    }

    pub fn occlusion_source_layer_id(&self) -> Option<u32> {
        self.nodes.iter().find_map(|node| match node.kind {
            NodeKind::OcclusionInput { source_layer_id } => Some(source_layer_id),
            _ => None,
        })
    }

    pub fn has_occlusion_input(&self) -> bool {
        self.occlusion_source_layer_id().is_some()
    }

    pub fn set_occlusion_source_layer_id(&mut self, source_layer_id: Option<u32>) -> bool {
        let Some((seed, scale)) = self.nodes.iter().find_map(|node| match node.kind {
            NodeKind::ValueNoise { seed, scale } => Some((seed, scale)),
            _ => None,
        }) else {
            return false;
        };
        let current = self.occlusion_source_layer_id();
        if current == source_layer_id {
            return false;
        }
        *self = match source_layer_id.filter(|id| *id != 0) {
            Some(layer_id) => Self::value_noise_times_occlusion(seed, scale, layer_id),
            None => Self::value_noise(seed, scale),
        };
        true
    }

    /// Reject malformed or oversized graphs before any evaluation allocation.
    pub fn validate(&self) -> Result<()> {
        if self.nodes.is_empty() || self.nodes.len() > MAX_GRAPH_NODES {
            bail!("Procedural graph node count is outside the supported limit");
        }
        if self.edges.len() > MAX_GRAPH_EDGES {
            bail!("Procedural graph edge count exceeds the supported limit");
        }

        let mut nodes = HashMap::with_capacity(self.nodes.len());
        let mut indegree = HashMap::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if nodes.insert(node.id, &node.kind).is_some() {
                bail!("Procedural graph contains duplicate node IDs");
            }
            indegree.insert(node.id, 0usize);
            if let NodeKind::ValueNoise { scale, .. } = node.kind {
                if !scale.is_finite() || !(MIN_NOISE_SCALE..=MAX_NOISE_SCALE).contains(&scale) {
                    bail!("Value noise scale is outside the finite supported range");
                }
            }
        }

        let mut edges = HashSet::with_capacity(self.edges.len());
        let mut outgoing: HashMap<u32, Vec<u32>> = HashMap::with_capacity(self.nodes.len());
        for edge in &self.edges {
            if edge.from == edge.to
                || !nodes.contains_key(&edge.from)
                || !nodes.contains_key(&edge.to)
                || !edges.insert(*edge)
            {
                bail!("Procedural graph contains an invalid or duplicate edge");
            }
            *indegree.get_mut(&edge.to).expect("validated target node") += 1;
            outgoing.entry(edge.from).or_default().push(edge.to);
        }

        let uv_nodes = self
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Uv))
            .count();
        let noise_nodes = self
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::ValueNoise { .. }))
            .count();
        let output_nodes = self
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::RoughnessOutput))
            .count();
        let occlusion_nodes = self
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::OcclusionInput { .. }))
            .count();
        let multiply_nodes = self
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Multiply))
            .count();
        if (uv_nodes, noise_nodes, output_nodes) != (1, 1, 1)
            || occlusion_nodes > 1
            || multiply_nodes != occlusion_nodes
        {
            bail!("Graph must contain UV, Value Noise and Roughness Output, with at most one complete Occlusion input");
        }
        if self
            .nodes
            .iter()
            .any(|node| matches!(node.kind, NodeKind::OcclusionInput { source_layer_id: 0 }))
        {
            bail!("Occlusion graph input requires a nonzero source layer ID");
        }

        let mut ready: Vec<u32> = indegree
            .iter()
            .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
            .collect();
        let mut visited = 0usize;
        while let Some(id) = ready.pop() {
            visited += 1;
            if let Some(targets) = outgoing.get(&id) {
                for target in targets {
                    let degree = indegree.get_mut(target).expect("validated target node");
                    *degree -= 1;
                    if *degree == 0 {
                        ready.push(*target);
                    }
                }
            }
        }
        if visited != self.nodes.len() {
            bail!("Procedural graph contains a cycle");
        }

        for node in &self.nodes {
            // Kahn's algorithm consumes indegrees above, so verify the fixed
            // type contract directly from the original edge list instead.
            let count = self.edges.iter().filter(|edge| edge.to == node.id).count();
            let valid_count = match node.kind {
                NodeKind::Uv | NodeKind::OcclusionInput { .. } => count == 0,
                NodeKind::ValueNoise { .. } => count == 1,
                NodeKind::Multiply => count == 2,
                NodeKind::RoughnessOutput => count == 1,
            };
            if !valid_count {
                bail!("Procedural graph node has a missing or unsupported input count");
            }
        }
        let by_id: HashMap<u32, &NodeKind> = self
            .nodes
            .iter()
            .map(|node| (node.id, &node.kind))
            .collect();
        for edge in &self.edges {
            let compatible = matches!(
                (by_id.get(&edge.from), by_id.get(&edge.to)),
                (Some(NodeKind::Uv), Some(NodeKind::ValueNoise { .. }))
                    | (Some(NodeKind::ValueNoise { .. }), Some(NodeKind::Multiply))
                    | (
                        Some(NodeKind::OcclusionInput { .. }),
                        Some(NodeKind::Multiply)
                    )
                    | (Some(NodeKind::Multiply), Some(NodeKind::RoughnessOutput))
                    | (
                        Some(NodeKind::ValueNoise { .. }),
                        Some(NodeKind::RoughnessOutput)
                    )
            );
            if !compatible {
                bail!("Procedural graph edge connects incompatible node types");
            }
        }
        if self.edges.len() != if occlusion_nodes == 1 { 4 } else { 2 } {
            bail!("Roughness graph has an incomplete or unsupported connection topology");
        }
        if occlusion_nodes == 1 {
            let multiply_id = self
                .nodes
                .iter()
                .find(|node| matches!(node.kind, NodeKind::Multiply))
                .map(|node| node.id)
                .expect("validated multiply node");
            let output_id = self
                .nodes
                .iter()
                .find(|node| matches!(node.kind, NodeKind::RoughnessOutput))
                .map(|node| node.id)
                .expect("validated output node");
            let noise_id = self
                .nodes
                .iter()
                .find(|node| matches!(node.kind, NodeKind::ValueNoise { .. }))
                .map(|node| node.id)
                .expect("validated noise node");
            let ao_id = self
                .nodes
                .iter()
                .find(|node| matches!(node.kind, NodeKind::OcclusionInput { .. }))
                .map(|node| node.id)
                .expect("validated AO node");
            if !self.edges.contains(&GraphEdge {
                from: noise_id,
                to: multiply_id,
            }) || !self.edges.contains(&GraphEdge {
                from: ao_id,
                to: multiply_id,
            }) || !self.edges.contains(&GraphEdge {
                from: multiply_id,
                to: output_id,
            }) {
                bail!("AO Roughness graph connections do not match the supported topology");
            }
        } else {
            let noise_id = self
                .nodes
                .iter()
                .find(|node| matches!(node.kind, NodeKind::ValueNoise { .. }))
                .expect("validated noise node")
                .id;
            let output_id = self
                .nodes
                .iter()
                .find(|node| matches!(node.kind, NodeKind::RoughnessOutput))
                .expect("validated output node")
                .id;
            if !self.edges.contains(&GraphEdge {
                from: noise_id,
                to: output_id,
            }) {
                bail!("Roughness graph has no connection from Value Noise to its output");
            }
        }
        Ok(())
    }

    /// Evaluate the graph at one UV coordinate, returning normalized roughness.
    pub fn evaluate_roughness(&self, uv: [f32; 2]) -> Result<f32> {
        self.evaluate_roughness_with_occlusion(uv, 1.0)
    }

    /// Evaluate Roughness with a normalized resolved Occlusion sample.
    pub fn evaluate_roughness_with_occlusion(&self, uv: [f32; 2], occlusion: f32) -> Result<f32> {
        self.validate()?;
        if !uv[0].is_finite() || !uv[1].is_finite() {
            bail!("Procedural graph UV input must be finite");
        }
        let (seed, scale) = self.noise_parameters();
        if !occlusion.is_finite() || !(0.0..=1.0).contains(&occlusion) {
            bail!("Procedural graph Occlusion input must be finite and normalized");
        }
        let noise = value_noise_2d(uv, seed, scale)?;
        Ok(if self.occlusion_source_layer_id().is_some() {
            noise * occlusion
        } else {
            noise
        })
    }

    /// Render the Roughness output at texel centers within a bounded map size.
    pub fn render_roughness(&self, size: u32) -> Result<RgbaImage> {
        if size == 0 || size > MAX_GRAPH_OUTPUT_SIZE {
            bail!("Procedural graph output size is outside the supported range");
        }
        let output_len = (size as usize)
            .checked_mul(size as usize)
            .and_then(|texels| texels.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("Procedural graph output size overflow"))?;
        let mut output_bytes = Vec::new();
        output_bytes.try_reserve_exact(output_len)?;
        output_bytes.resize(output_len, 255);
        for y in 0..size {
            let row = self.render_roughness_row(size, y)?;
            let start = y as usize * size as usize * 4;
            output_bytes[start..start + row.len()].copy_from_slice(&row);
        }
        ImageBuffer::from_raw(size, size, output_bytes)
            .ok_or_else(|| anyhow::anyhow!("Could not allocate procedural graph output"))
    }

    /// Render one scanline for allocation-bounded region compositing.
    pub fn render_roughness_row(&self, size: u32, y: u32) -> Result<Vec<u8>> {
        self.render_roughness_span(size, y, 0, size, None)
    }

    /// Render a horizontal span. `occlusion_rgba` is the full source image so
    /// bounded composites only allocate and evaluate the requested span.
    pub fn render_roughness_span(
        &self,
        size: u32,
        y: u32,
        x: u32,
        width: u32,
        occlusion_rgba: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        self.validate()?;
        if size == 0
            || size > MAX_GRAPH_OUTPUT_SIZE
            || y >= size
            || width == 0
            || x.checked_add(width).is_none_or(|end| end > size)
        {
            bail!("Procedural graph row is outside the supported range");
        }
        let source_id = self.occlusion_source_layer_id();
        if source_id.is_some() {
            let expected = (size as usize)
                .checked_mul(size as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or_else(|| anyhow::anyhow!("Procedural graph Occlusion input size overflow"))?;
            if occlusion_rgba.is_some_and(|bytes| bytes.len() != expected) {
                bail!("Procedural graph Occlusion input dimensions do not match the output");
            }
        }
        let row_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Procedural graph row size overflow"))?;
        let mut row = Vec::new();
        row.try_reserve_exact(row_bytes)?;
        let (seed, scale) = self.noise_parameters();
        let v = (y as f32 + 0.5) / size as f32;
        for offset_x in 0..width {
            let pixel_x = x + offset_x;
            let uv = [(pixel_x as f32 + 0.5) / size as f32, v];
            let noise = value_noise_2d(uv, seed, scale)?;
            let ao = source_id.and(occlusion_rgba).map_or(1.0, |bytes| {
                bytes[((y * size + pixel_x) as usize) * 4] as f32 / 255.0
            });
            let value = if source_id.is_some() {
                noise * ao
            } else {
                noise
            };
            let roughness = (value * 255.0).round().clamp(0.0, 255.0) as u8;
            row.extend_from_slice(&[roughness, roughness, roughness, 255]);
        }
        Ok(row)
    }

    fn noise_parameters(&self) -> (u64, f32) {
        self.nodes
            .iter()
            .find_map(|node| match node.kind {
                NodeKind::ValueNoise { seed, scale } => Some((seed, scale)),
                _ => None,
            })
            .expect("validated graph has a noise node")
    }
}

fn value_noise_2d(uv: [f32; 2], seed: u64, scale: f32) -> Result<f32> {
    let x = uv[0] * scale;
    let y = uv[1] * scale;
    if !x.is_finite() || !y.is_finite() {
        bail!("Scaled procedural graph coordinates must remain finite");
    }
    let x0 = x.floor() as i64;
    let y0 = y.floor() as i64;
    let tx = smoothstep(x - x.floor());
    let ty = smoothstep(y - y.floor());
    let top = lerp(
        lattice_value(x0, y0, seed),
        lattice_value(x0 + 1, y0, seed),
        tx,
    );
    let bottom = lerp(
        lattice_value(x0, y0 + 1, seed),
        lattice_value(x0 + 1, y0 + 1, seed),
        tx,
    );
    Ok(lerp(top, bottom, ty))
}

fn lattice_value(x: i64, y: i64, seed: u64) -> f32 {
    let mut value = seed
        ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    (value >> 40) as f32 / ((1u32 << 24) - 1) as f32
}

fn smoothstep(value: f32) -> f32 {
    value * value * (3.0 - 2.0 * value)
}

fn lerp(a: f32, b: f32, weight: f32) -> f32 {
    a + (b - a) * weight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_value_noise_is_deterministic_and_seeded() {
        let graph = ProceduralGraph::value_noise(7, 8.0);
        let point = [0.237, 0.619];
        let first = graph.evaluate_roughness(point).unwrap();
        assert_eq!(first, graph.evaluate_roughness(point).unwrap());
        assert!((0.0..=1.0).contains(&first));
        assert_ne!(
            first,
            ProceduralGraph::value_noise(8, 8.0)
                .evaluate_roughness(point)
                .unwrap()
        );
        let image = graph.render_roughness(32).unwrap();
        assert_eq!(image, graph.render_roughness(32).unwrap());
        assert!(image.pixels().all(|pixel| pixel.0[0] == pixel.0[1]
            && pixel.0[1] == pixel.0[2]
            && pixel.0[3] == 255));
    }

    #[test]
    fn ao_input_graph_multiplies_seeded_noise_and_can_be_detached() {
        let mut graph = ProceduralGraph::value_noise_times_occlusion(71, 4.0, 19);
        graph.validate().unwrap();
        let uv = [0.375, 0.625];
        let noise = ProceduralGraph::value_noise(71, 4.0)
            .evaluate_roughness(uv)
            .unwrap();
        assert_eq!(
            graph.evaluate_roughness_with_occlusion(uv, 0.5).unwrap(),
            noise * 0.5
        );
        assert_eq!(graph.evaluate_roughness(uv).unwrap(), noise);
        assert_eq!(graph.occlusion_source_layer_id(), Some(19));
        assert!(graph.set_occlusion_source_layer_id(None));
        assert_eq!(graph.occlusion_source_layer_id(), None);
        assert_eq!(graph.evaluate_roughness(uv).unwrap(), noise);
        graph.validate().unwrap();
    }

    #[test]
    fn ao_input_graph_rejects_zero_layer_ids_and_broken_topology() {
        let mut graph = ProceduralGraph::value_noise_times_occlusion(1, 2.0, 4);
        if let NodeKind::OcclusionInput { source_layer_id } = &mut graph.nodes[2].kind {
            *source_layer_id = 0;
        }
        assert!(graph.validate().is_err());

        let mut graph = ProceduralGraph::value_noise_times_occlusion(1, 2.0, 4);
        graph.edges.pop();
        assert!(graph.validate().is_err());
    }

    #[test]
    fn ao_span_uses_full_texture_coordinates_and_missing_source_is_neutral() {
        let graph = ProceduralGraph::value_noise_times_occlusion(23, 5.0, 2);
        let source = vec![128u8; 64 * 64 * 4];
        let span = graph
            .render_roughness_span(64, 17, 11, 9, Some(&source))
            .unwrap();
        let noise = ProceduralGraph::value_noise(23, 5.0);
        for local_x in 0..9 {
            let uv = [((local_x + 11) as f32 + 0.5) / 64.0, 17.5 / 64.0];
            let expected =
                (noise.evaluate_roughness(uv).unwrap() * (128.0 / 255.0) * 255.0).round() as u8;
            assert_eq!(span[local_x as usize * 4], expected);
        }
        assert_eq!(
            graph.render_roughness_row(64, 17).unwrap(),
            noise.render_roughness_row(64, 17).unwrap()
        );
        assert!(graph
            .render_roughness_span(64, 17, 0, 64, Some(&[0; 4]))
            .is_err());
    }

    #[test]
    fn graph_rejects_non_finite_scale_uv_and_missing_connections() {
        assert!(ProceduralGraph::value_noise(0, f32::NAN)
            .validate()
            .is_err());
        assert!(ProceduralGraph::value_noise(0, 1.0)
            .evaluate_roughness([f32::INFINITY, 0.0])
            .is_err());
        assert!(ProceduralGraph::value_noise(0, 2.0)
            .evaluate_roughness([f32::MAX, f32::MAX])
            .is_err());
        assert!(ProceduralGraph::value_noise(0, 1.0)
            .render_roughness(MAX_GRAPH_OUTPUT_SIZE + 1)
            .is_err());
        let mut graph = ProceduralGraph::value_noise(0, 1.0);
        graph.edges.pop();
        assert!(graph.validate().is_err());
    }

    #[test]
    fn graph_rejects_cycles_duplicate_ids_and_excessive_nodes() {
        let mut graph = ProceduralGraph::value_noise(0, 1.0);
        graph.edges.push(GraphEdge { from: 3, to: 1 });
        assert!(graph.validate().is_err());

        let mut graph = ProceduralGraph::value_noise(0, 1.0);
        graph.nodes[1].id = graph.nodes[0].id;
        assert!(graph.validate().is_err());

        let mut graph = ProceduralGraph::value_noise(0, 1.0);
        graph
            .nodes
            .resize(MAX_GRAPH_NODES + 1, graph.nodes[0].clone());
        assert!(graph.validate().is_err());
    }
}

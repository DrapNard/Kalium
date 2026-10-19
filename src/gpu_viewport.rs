//! Experimental native WGPU mesh preview. The CPU [`crate::renderer::Viewport`]
//! remains authoritative for picking, painting and the software fallback.
use crate::document::{Channel, Document};
use anyhow::{anyhow, ensure, Context, Result};
use eframe::{egui, egui_wgpu, wgpu};
use glam::{Mat4, Vec3};
use image::RgbaImage;
use std::collections::BTreeMap;

const MAX_GPU_SCENE_BYTES: usize = 256 * 1024 * 1024;
const MAX_GPU_TRIANGLES: usize = 350_000;
const MAX_GPU_REGION_UPDATES: usize = 1024;
const MAX_GPU_REGION_UPDATE_BYTES: usize = 16 * 1024 * 1024;

struct PreparedGroup {
    texture_set_id: u32,
    indices: Vec<u32>,
}

struct PreparedScene {
    vertices: Vec<f32>,
    groups: Vec<PreparedGroup>,
}

/// Pack mesh data and group index streams by bound Texture Set. This is kept
/// CPU-side so import bounds and routing can be tested without a GPU adapter.
fn prepare_scene(doc: &Document) -> Result<PreparedScene> {
    let mesh = &doc.mesh;
    ensure!(
        mesh.indices.len().is_multiple_of(3),
        "mesh index count is not triangular"
    );
    let triangle_count = mesh.indices.len() / 3;
    ensure!(
        triangle_count <= MAX_GPU_TRIANGLES,
        "mesh exceeds GPU preview triangle cap"
    );

    let mut vertices = Vec::new();
    let vertex_floats = mesh
        .vertices
        .len()
        .checked_mul(8)
        .context("GPU vertex count overflow")?;
    let vertex_bytes = vertex_floats
        .checked_mul(std::mem::size_of::<f32>())
        .context("GPU vertex bytes overflow")?;
    ensure!(
        vertex_bytes <= MAX_GPU_SCENE_BYTES,
        "mesh exceeds GPU preview memory cap"
    );
    vertices
        .try_reserve_exact(vertex_floats)
        .map_err(|_| anyhow!("cannot allocate GPU preview vertices"))?;
    for vertex in &mesh.vertices {
        ensure!(
            vertex
                .position
                .iter()
                .chain(vertex.normal.iter())
                .chain(vertex.uv.iter())
                .all(|value| value.is_finite()),
            "mesh contains non-finite GPU preview attributes"
        );
        vertices.extend_from_slice(&vertex.position);
        vertices.extend_from_slice(&vertex.normal);
        vertices.extend_from_slice(&vertex.uv);
    }

    let mut by_set = BTreeMap::<u32, Vec<u32>>::new();
    for (triangle_index, triangle) in mesh.indices.chunks_exact(3).enumerate() {
        ensure!(
            triangle
                .iter()
                .all(|index| (*index as usize) < mesh.vertices.len()),
            "mesh has an out-of-range GPU preview index"
        );
        let slot = mesh.triangle_material(triangle_index);
        let set_id = doc.texture_set_for_material(slot);
        let indices = by_set.entry(set_id).or_default();
        indices
            .try_reserve(3)
            .map_err(|_| anyhow!("cannot allocate GPU preview indices"))?;
        indices.extend_from_slice(triangle);
    }
    Ok(PreparedScene {
        vertices,
        groups: by_set
            .into_iter()
            .map(|(texture_set_id, indices)| PreparedGroup {
                texture_set_id,
                indices,
            })
            .collect(),
    })
}

pub struct GpuViewport {
    render_state: egui_wgpu::RenderState,
    pipeline: wgpu::RenderPipeline,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    vertex_buffer: Option<wgpu::Buffer>,
    draw_groups: Vec<GpuDrawGroup>,
    scene_ready: bool,
    target: GpuTarget,
}

struct GpuTarget {
    _color: wgpu::Texture,
    color_view: wgpu::TextureView,
    _depth: wgpu::Texture,
    depth_view: wgpu::TextureView,
    texture_id: egui::TextureId,
    size: [u32; 2],
}

struct GpuDrawGroup {
    texture_set_id: u32,
    channel_size: [u32; 2],
    textures: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
    index_buffer: wgpu::Buffer,
    index_count: u32,
}

impl GpuViewport {
    pub fn new(render_state: egui_wgpu::RenderState) -> Result<Self> {
        let device = &render_state.device;
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kalium_gpu_camera_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(80),
                },
                count: None,
            }],
        });
        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("kalium_gpu_base_color_layout"),
                entries: &std::iter::once(wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                })
                .chain((1..=8).map(|binding| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }))
                .collect::<Vec<_>>(),
            });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kalium_gpu_viewport_pipeline_layout"),
            bind_group_layouts: &[&camera_layout, &texture_bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kalium_gpu_viewport_shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("../assets/shaders/viewport.wgsl").into(),
            ),
        });
        let attributes = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kalium_gpu_viewport_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 8 * 4,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attributes,
                }],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::LessEqual,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });
        let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kalium_gpu_viewport_camera"),
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kalium_gpu_viewport_camera_bind_group"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });
        let target = Self::create_target(&render_state, [600, 600], None);
        Ok(Self {
            render_state,
            pipeline,
            camera_buffer,
            camera_bind_group,
            texture_bind_group_layout,
            vertex_buffer: None,
            draw_groups: Vec::new(),
            scene_ready: false,
            target,
        })
    }

    fn create_target(
        render_state: &egui_wgpu::RenderState,
        size: [u32; 2],
        old_id: Option<egui::TextureId>,
    ) -> GpuTarget {
        let color = render_state
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("kalium_gpu_viewport_color"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let texture_id = {
            let mut renderer = render_state.renderer.write();
            if let Some(texture_id) = old_id {
                renderer.update_egui_texture_from_wgpu_texture(
                    &render_state.device,
                    &color_view,
                    wgpu::FilterMode::Linear,
                    texture_id,
                );
                texture_id
            } else {
                renderer.register_native_texture(
                    &render_state.device,
                    &color_view,
                    wgpu::FilterMode::Linear,
                )
            }
        };
        let depth = render_state
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("kalium_gpu_viewport_depth"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        GpuTarget {
            _color: color,
            color_view,
            _depth: depth,
            depth_view,
            texture_id,
            size,
        }
    }

    pub fn texture_id(&self) -> egui::TextureId {
        self.target.texture_id
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        let size = [width.clamp(128, 1100) as u32, height.clamp(128, 900) as u32];
        if self.target.size != size {
            self.target =
                Self::create_target(&self.render_state, size, Some(self.target.texture_id));
        }
    }

    pub fn update_scene(&mut self, doc: &Document) -> Result<()> {
        let prepared = prepare_scene(doc)?;
        let vertex_bytes = f32_bytes(&prepared.vertices);
        ensure!(
            vertex_bytes.len() <= MAX_GPU_SCENE_BYTES,
            "mesh exceeds GPU preview memory cap"
        );
        let device = &self.render_state.device;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kalium_gpu_viewport_vertices"),
            size: vertex_bytes.len().max(4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if !vertex_bytes.is_empty() {
            self.render_state
                .queue
                .write_buffer(&vertex_buffer, 0, &vertex_bytes);
        }
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kalium_gpu_viewport_base_color_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let mut uploaded_bytes = vertex_bytes.len();
        let mut draw_groups = Vec::with_capacity(prepared.groups.len());
        for group in prepared.groups {
            let mut textures = Vec::with_capacity(PREVIEW_CHANNELS.len());
            let mut channel_size = [0, 0];
            for channel in PREVIEW_CHANNELS {
                let image = doc
                    .composite_for_set(group.texture_set_id, channel)
                    .ok_or_else(|| {
                        anyhow!(
                            "Texture Set {} has no {} map",
                            group.texture_set_id,
                            channel.label()
                        )
                    })?;
                ensure!(
                    image.width() > 0 && image.height() > 0,
                    "GPU material texture is empty"
                );
                ensure!(
                    image.width() <= device.limits().max_texture_dimension_2d
                        && image.height() <= device.limits().max_texture_dimension_2d,
                    "GPU material texture exceeds device dimensions"
                );
                let size = [image.width(), image.height()];
                if textures.is_empty() {
                    channel_size = size;
                }
                ensure!(
                    size == channel_size,
                    "GPU material channels have inconsistent dimensions"
                );
                uploaded_bytes = uploaded_bytes
                    .checked_add(image.as_raw().len())
                    .context("GPU scene byte count overflow")?;
                ensure!(
                    uploaded_bytes <= MAX_GPU_SCENE_BYTES,
                    "GPU preview textures exceed memory cap"
                );
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("kalium_gpu_material_channel"),
                    size: wgpu::Extent3d {
                        width: size[0],
                        height: size[1],
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: channel_texture_format(channel),
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                self.render_state.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    image.as_raw(),
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(size[0] * 4),
                        rows_per_image: Some(size[1]),
                    },
                    wgpu::Extent3d {
                        width: size[0],
                        height: size[1],
                        depth_or_array_layers: 1,
                    },
                );
                textures.push(texture);
            }
            let views = textures
                .iter()
                .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
                .collect::<Vec<_>>();
            let mut entries = vec![wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(&sampler),
            }];
            entries.extend(
                views
                    .iter()
                    .enumerate()
                    .map(|(index, view)| wgpu::BindGroupEntry {
                        binding: index as u32 + 1,
                        resource: wgpu::BindingResource::TextureView(view),
                    }),
            );
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kalium_gpu_material_bind_group"),
                layout: &self.texture_bind_group_layout,
                entries: &entries,
            });
            let index_bytes = u32_bytes(&group.indices);
            uploaded_bytes = uploaded_bytes
                .checked_add(index_bytes.len())
                .context("GPU scene byte count overflow")?;
            ensure!(
                uploaded_bytes <= MAX_GPU_SCENE_BYTES,
                "GPU preview geometry exceeds memory cap"
            );
            let index_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("kalium_gpu_viewport_indices"),
                size: index_bytes.len().max(4) as u64,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            if !index_bytes.is_empty() {
                self.render_state
                    .queue
                    .write_buffer(&index_buffer, 0, &index_bytes);
            }
            draw_groups.push(GpuDrawGroup {
                texture_set_id: group.texture_set_id,
                channel_size,
                textures,
                bind_group,
                index_buffer,
                index_count: group.indices.len() as u32,
            });
        }
        self.vertex_buffer = Some(vertex_buffer);
        self.draw_groups = draw_groups;
        self.scene_ready = true;
        Ok(())
    }

    /// Upload a composited region for any preview channel. Validation completes
    /// before writes, with the same bounds as Base Color updates.
    pub fn update_channel_regions(
        &mut self,
        texture_set_id: u32,
        channel: Channel,
        regions: &[(u32, u32, &RgbaImage)],
    ) -> Result<bool> {
        let channel_index = PREVIEW_CHANNELS
            .iter()
            .position(|candidate| *candidate == channel)
            .context("unsupported GPU preview channel")?;
        if !contains_texture_set(
            self.draw_groups.iter().map(|group| group.texture_set_id),
            texture_set_id,
        ) {
            return Ok(false);
        }
        let matching_groups = self
            .draw_groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.texture_set_id == texture_set_id)
            .map(|(index, group)| (index, group.channel_size))
            .collect::<Vec<_>>();
        if matching_groups.is_empty() {
            return Ok(false);
        }

        let primary_size = matching_groups[0].1;
        let validated = validate_base_color_regions(primary_size, regions)?;
        ensure!(
            matching_groups
                .iter()
                .all(|(_, size)| *size == primary_size),
            "resident draw groups for a Texture Set have inconsistent material channel dimensions"
        );

        for (group_index, _) in matching_groups {
            let texture = &self.draw_groups[group_index].textures[channel_index];
            for (region, validated_region) in regions.iter().zip(&validated) {
                self.render_state.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: validated_region.origin[0],
                            y: validated_region.origin[1],
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    region.2.as_raw(),
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(validated_region.bytes_per_row),
                        rows_per_image: Some(validated_region.extent[1]),
                    },
                    wgpu::Extent3d {
                        width: validated_region.extent[0],
                        height: validated_region.extent[1],
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        Ok(true)
    }

    pub fn render(&self, yaw: f32, pitch: f32, distance: f32, clear: [f64; 4]) -> Result<()> {
        if !self.scene_ready {
            return Ok(());
        }
        let aspect = self.target.size[0] as f32 / self.target.size[1] as f32;
        let matrix = camera_matrix(yaw, pitch, distance, aspect);
        let mut uniforms = matrix.to_cols_array().to_vec();
        uniforms.extend_from_slice(&camera_eye(yaw, pitch, distance).extend(1.0).to_array());
        self.render_state
            .queue
            .write_buffer(&self.camera_buffer, 0, &f32_bytes(&uniforms));
        let mut encoder =
            self.render_state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("kalium_gpu_viewport_encoder"),
                });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kalium_gpu_viewport_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target.color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0],
                            g: clear[1],
                            b: clear[2],
                            a: clear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.target.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.camera_bind_group, &[]);
            if let Some(vertex_buffer) = &self.vertex_buffer {
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                for group in &self.draw_groups {
                    if group.index_count == 0 {
                        continue;
                    }
                    pass.set_bind_group(1, &group.bind_group, &[]);
                    pass.set_index_buffer(group.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..group.index_count, 0, 0..1);
                }
            }
        }
        self.render_state.queue.submit(Some(encoder.finish()));
        Ok(())
    }
}

// Binding order matches viewport.wgsl; data maps must never be sRGB decoded.
const PREVIEW_CHANNELS: [Channel; 8] = [
    Channel::BaseColor,
    Channel::Roughness,
    Channel::Metallic,
    Channel::Normal,
    Channel::Emissive,
    Channel::Height,
    Channel::Occlusion,
    Channel::Opacity,
];
fn channel_texture_format(channel: Channel) -> wgpu::TextureFormat {
    match channel {
        Channel::BaseColor | Channel::Emissive => wgpu::TextureFormat::Rgba8UnormSrgb,
        _ => wgpu::TextureFormat::Rgba8Unorm,
    }
}

fn camera_eye(yaw: f32, pitch: f32, distance: f32) -> Vec3 {
    let yaw = if yaw.is_finite() { yaw } else { 0.42 };
    let pitch = if pitch.is_finite() {
        pitch.clamp(-1.45, 1.45)
    } else {
        0.25
    };
    let distance = if distance.is_finite() {
        distance.clamp(1.6, 15.0)
    } else {
        5.4
    };
    Vec3::new(
        distance * yaw.sin() * pitch.cos(),
        distance * pitch.sin(),
        distance * yaw.cos() * pitch.cos(),
    )
}

fn camera_matrix(yaw: f32, pitch: f32, distance: f32, aspect: f32) -> Mat4 {
    let aspect = if aspect.is_finite() {
        aspect.clamp(0.1, 10.0)
    } else {
        1.0
    };
    let eye = camera_eye(yaw, pitch, distance);
    let view = Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);
    let vertical_fov = 2.0 * (1.0 / 2.16f32).atan();
    Mat4::perspective_rh(vertical_fov, aspect, 0.05, 100.0) * view
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

fn u32_bytes(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ValidatedRegion {
    origin: [u32; 2],
    extent: [u32; 2],
    bytes_per_row: u32,
}

fn validate_base_color_regions(
    texture_size: [u32; 2],
    regions: &[(u32, u32, &RgbaImage)],
) -> Result<Vec<ValidatedRegion>> {
    ensure!(!regions.is_empty(), "Base Color region batch is empty");
    ensure!(
        regions.len() <= MAX_GPU_REGION_UPDATES,
        "Base Color region batch exceeds the update count cap"
    );
    ensure!(
        texture_size[0] > 0 && texture_size[1] > 0,
        "resident Base Color texture has an empty extent"
    );

    let mut total_bytes = 0usize;
    let mut validated = Vec::new();
    validated
        .try_reserve_exact(regions.len())
        .map_err(|_| anyhow!("cannot allocate GPU region validation records"))?;
    for (x, y, image) in regions {
        let width = image.width();
        let height = image.height();
        ensure!(width > 0 && height > 0, "Base Color region is empty");

        let right = x
            .checked_add(width)
            .context("Base Color region x extent overflow")?;
        let bottom = y
            .checked_add(height)
            .context("Base Color region y extent overflow")?;
        ensure!(
            right <= texture_size[0] && bottom <= texture_size[1],
            "Base Color region exceeds resident texture bounds"
        );

        let expected_len = usize::try_from(width)
            .context("Base Color region width does not fit memory size")?
            .checked_mul(usize::try_from(height).context("Base Color region height overflow")?)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("Base Color region byte count overflow")?;
        ensure!(
            image.as_raw().len() == expected_len,
            "Base Color region RGBA byte length does not match its dimensions"
        );
        total_bytes = total_bytes
            .checked_add(expected_len)
            .context("Base Color region batch byte count overflow")?;
        ensure!(
            total_bytes <= MAX_GPU_REGION_UPDATE_BYTES,
            "Base Color region batch exceeds the upload byte cap"
        );
        let bytes_per_row = width
            .checked_mul(4)
            .context("Base Color region row byte count overflow")?;
        validated.push(ValidatedRegion {
            origin: [*x, *y],
            extent: [width, height],
            bytes_per_row,
        });
    }
    Ok(validated)
}

fn contains_texture_set(texture_set_ids: impl IntoIterator<Item = u32>, target: u32) -> bool {
    texture_set_ids.into_iter().any(|id| id == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_shader_parses_and_validates_without_gpu_hardware() {
        let source = include_str!("../assets/shaders/viewport.wgsl");
        let module = naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|error| panic!("{}", error.emit_to_string(source)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("viewport WGSL should validate");
    }

    #[test]
    fn preview_channels_cover_every_editable_map_with_correct_color_space() {
        assert_eq!(PREVIEW_CHANNELS.len(), Channel::ALL.len());
        for channel in Channel::ALL {
            assert_eq!(
                PREVIEW_CHANNELS
                    .iter()
                    .filter(|entry| **entry == channel)
                    .count(),
                1
            );
            let expected = if matches!(channel, Channel::BaseColor | Channel::Emissive) {
                wgpu::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            };
            assert_eq!(channel_texture_format(channel), expected);
        }
    }

    #[test]
    fn shader_bindings_match_all_material_upload_slots_and_camera_size() {
        let source = include_str!("../assets/shaders/viewport.wgsl");
        let module = naga::front::wgsl::parse_str(source).unwrap();
        let mut material_bindings = module
            .global_variables
            .iter()
            .filter_map(|(_, value)| {
                value
                    .binding
                    .as_ref()
                    .filter(|binding| binding.group == 1)
                    .map(|binding| binding.binding)
            })
            .collect::<Vec<_>>();
        material_bindings.sort_unstable();
        assert_eq!(material_bindings, (0..=8).collect::<Vec<_>>());
        let camera = module
            .global_variables
            .iter()
            .find(|(_, variable)| {
                variable
                    .binding
                    .as_ref()
                    .is_some_and(|binding| binding.group == 0 && binding.binding == 0)
            })
            .unwrap()
            .1;
        match &module.types[camera.ty].inner {
            naga::TypeInner::Struct { span, .. } => assert_eq!(*span, 80),
            _ => panic!("camera uniform must remain a struct"),
        }
        assert!(camera_eye(f32::NAN, f32::INFINITY, -1.0).is_finite());
    }

    #[test]
    fn gpu_camera_is_finite_and_tracks_orbit_zoom_and_aspect() {
        let initial = camera_matrix(0.42, 0.25, 5.4, 1.0);
        assert!(initial
            .to_cols_array()
            .iter()
            .all(|value| value.is_finite()));
        assert_ne!(initial, camera_matrix(1.1, -0.3, 5.4, 1.0));
        assert_ne!(initial, camera_matrix(0.42, 0.25, 6.0, 1.0));
        assert_ne!(initial, camera_matrix(0.42, 0.25, 5.4, 1.7));
        assert!(camera_matrix(f32::NAN, f32::INFINITY, f32::NAN, 0.0)
            .to_cols_array()
            .iter()
            .all(|value| value.is_finite()));
    }

    #[test]
    fn scene_packs_uvs_and_routes_triangles_by_bound_set() {
        let mut doc = Document::new(64);
        let first = doc.active_set_id;
        let second = doc.add_texture_set("Metal").unwrap();
        doc.mesh.materials = vec!["Leather".into(), "Metal".into()];
        doc.mesh.triangle_materials = vec![0, 1];
        doc.mesh.indices = vec![0, 1, 2, 3, 4, 5];
        doc.mesh.vertices = (0..6)
            .map(|index| crate::mesh::Vertex {
                position: [index as f32, 0.0, 0.0],
                normal: [0.0, 0.0, 1.0],
                uv: [index as f32 / 5.0, 0.25],
                tangent: None,
                extensions: Default::default(),
            })
            .collect();
        doc.material_bindings = vec![Some(first), Some(second)];
        let scene = prepare_scene(&doc).unwrap();
        assert_eq!(&scene.vertices[6..8], &[0.0, 0.25]);
        assert_eq!(scene.groups.len(), 2);
        assert_eq!(scene.groups[0].texture_set_id, first);
        assert_eq!(scene.groups[0].indices, [0, 1, 2]);
        assert_eq!(scene.groups[1].texture_set_id, second);
        assert_eq!(scene.groups[1].indices, [3, 4, 5]);
    }

    #[test]
    fn scene_rejects_nonfinite_vertices_bad_indices_and_nontriangles() {
        let mut doc = Document::new(64);
        doc.mesh.vertices[0].position[0] = f32::NAN;
        assert!(prepare_scene(&doc).is_err());
        doc.mesh.vertices[0].position[0] = 0.0;
        doc.mesh.indices[0] = u32::MAX;
        assert!(prepare_scene(&doc).is_err());
        doc.mesh.indices.pop();
        assert!(prepare_scene(&doc).is_err());
    }

    #[test]
    fn region_validation_accepts_valid_in_bounds_rgba_region() {
        let image = RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 4]));
        let regions = [(4, 5, &image)];
        assert_eq!(
            validate_base_color_regions([8, 8], &regions).unwrap(),
            [ValidatedRegion {
                origin: [4, 5],
                extent: [3, 2],
                bytes_per_row: 12,
            }]
        );
    }

    #[test]
    fn region_validation_rejects_empty_batches_and_empty_regions() {
        assert!(validate_base_color_regions([8, 8], &[]).is_err());

        let empty = RgbaImage::new(0, 2);
        assert!(validate_base_color_regions([8, 8], &[(0, 0, &empty)]).is_err());
    }

    #[test]
    fn region_validation_rejects_out_of_bounds_and_overflowing_extents() {
        let image = RgbaImage::from_pixel(2, 2, image::Rgba([0; 4]));
        assert!(validate_base_color_regions([8, 8], &[(7, 0, &image)]).is_err());
        assert!(validate_base_color_regions([8, 8], &[(u32::MAX, 0, &image)]).is_err());
        assert!(validate_base_color_regions([0, 8], &[(0, 0, &image)]).is_err());
    }

    #[test]
    fn resident_texture_set_lookup_reports_missing_set() {
        assert!(contains_texture_set([2, 7, 11], 7));
        assert!(!contains_texture_set([2, 7, 11], 9));
        assert!(!contains_texture_set([], 9));
    }
}

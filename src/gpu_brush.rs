//! Opt-in GPU compute proof of concept for a single stamp.
//!
//! This is *not* wired to the interactive editor yet: synchronous readback
//! would be too slow for real-time painting. Enable with `gpu-experimental`.
//! No GPU adapter, shader or runtime compatibility has been validated in the
//! source-generation environment; failures are reported as errors.
use crate::paint::{Brush,BrushMode};
use anyhow::{bail,Context,Result};
use image::RgbaImage;
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone,Copy,bytemuck::Pod,bytemuck::Zeroable)]
struct StampParams {
    width:u32,
    height:u32,
    radius:f32,
    opacity:f32,
    hardness:f32,
    center_x:f32,
    center_y:f32,
    erase:u32,
    color:[u32;4],
}

pub fn stamp(image:&mut RgbaImage,brush:&Brush,uv:[f32;2])->Result<()> {
    brush.validate()?;
    if !matches!(brush.mode,BrushMode::Paint|BrushMode::Erase) {
        bail!("Experimental GPU brush currently supports only Paint and Erase");
    }
    if uv.iter().any(|v|!v.is_finite() || !(0.0..=1.0).contains(v)) {
        bail!("Invalid brush UV coordinate");
    }
    if image.width()==0 || image.height()==0 || image.width()>4096 || image.height()>4096 {
        bail!("Experimental GPU brush supports texture dimensions 1..=4096");
    }
    let params=StampParams {
        width:image.width(),height:image.height(),radius:brush.radius as f32,
        opacity:brush.opacity,hardness:brush.hardness,
        center_x:uv[0]*(image.width()-1) as f32,
        center_y:uv[1]*(image.height()-1) as f32,
        erase:u32::from(brush.mode==BrushMode::Erase),
        color:brush.color.map(u32::from),
    };
    // Packed little-endian RGBA8: exactly one u32 per pixel.
    let input:Vec<u32>=image.pixels().map(|p|u32::from_le_bytes(p.0)).collect();
    let instance=wgpu::Instance::default();
    let adapter=pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference:wgpu::PowerPreference::HighPerformance,
        compatible_surface:None,
        force_fallback_adapter:false,
    })).context("No compute-capable GPU adapter available")?;
    let (device,queue)=pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor::default(),None
    )).context("Failed to create GPU device")?;
    let shader=device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label:Some("Kalium compute stamp"),
        source:wgpu::ShaderSource::Wgsl(include_str!("../assets/shaders/paint.wgsl").into()),
    });
    let storage=device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label:Some("Kalium RGBA8 texels"),contents:bytemuck::cast_slice(&input),
        usage:wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    });
    let uniform=device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label:Some("Kalium brush parameters"),contents:bytemuck::bytes_of(&params),
        usage:wgpu::BufferUsages::UNIFORM,
    });
    let bytes=(input.len() as u64)*4;
    let readback=device.create_buffer(&wgpu::BufferDescriptor {
        label:Some("Kalium readback"),size:bytes,
        usage:wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation:false,
    });
    let layout=device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label:Some("Kalium stamp layout"),
        entries:&[
            wgpu::BindGroupLayoutEntry {
                binding:0,visibility:wgpu::ShaderStages::COMPUTE,
                ty:wgpu::BindingType::Buffer {ty:wgpu::BufferBindingType::Storage{read_only:false},
                    has_dynamic_offset:false,min_binding_size:None},count:None,
            },
            wgpu::BindGroupLayoutEntry {
                binding:1,visibility:wgpu::ShaderStages::COMPUTE,
                ty:wgpu::BindingType::Buffer {ty:wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset:false,min_binding_size:None},count:None,
            },
        ],
    });
    let bind=device.create_bind_group(&wgpu::BindGroupDescriptor {
        label:Some("Kalium stamp binding"),layout:&layout,
        entries:&[
            wgpu::BindGroupEntry {binding:0,resource:storage.as_entire_binding()},
            wgpu::BindGroupEntry {binding:1,resource:uniform.as_entire_binding()},
        ],
    });
    let pipeline_layout=device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label:Some("Kalium stamp pipeline layout"),bind_group_layouts:&[&layout],
        push_constant_ranges:&[],
    });
    let pipeline=device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label:Some("Kalium experimental stamp"),layout:Some(&pipeline_layout),
        module:&shader,entry_point:Some("main"),compilation_options:Default::default(),
        cache:None,
    });
    let mut encoder=device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label:Some("Kalium stamp commands"),
    });
    {
        let mut pass=encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label:Some("Kalium stamp pass"),timestamp_writes:None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0,&bind,&[]);
        pass.dispatch_workgroups(image.width().div_ceil(8),image.height().div_ceil(8),1);
    }
    encoder.copy_buffer_to_buffer(&storage,0,&readback,0,bytes);
    queue.submit(std::iter::once(encoder.finish()));
    let slice=readback.slice(..);
    let (tx,rx)=std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read,move |result| {let _=tx.send(result);});
    let _=device.poll(wgpu::Maintain::Wait);
    rx.recv().context("GPU readback was interrupted")?
        .context("GPU readback mapping failed")?;
    let mapped=slice.get_mapped_range();
    if mapped.len()!=image.as_raw().len() {
        bail!("GPU output byte length mismatch");
    }
    image.as_mut().copy_from_slice(&mapped);
    drop(mapped);
    readback.unmap();
    Ok(())
}

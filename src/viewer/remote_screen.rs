//! The remote screen's texture. Decoded frames arrive as NV12 and are
//! uploaded as two small textures (Y, and interleaved U/V), then converted
//! to RGB by a shader into a texture egui draws like any other image — 5 MB
//! uploaded per 1440p frame instead of 14 MB of RGBA, and no per-pixel
//! colour conversion on the CPU. Phase 5 of `docs/hardware-encoding-plan.md`.
//!
//! Without wgpu (another renderer, which this build doesn't enable) it falls
//! back to converting on the CPU and uploading RGBA, as before.

use crate::codec::Nv12Frame;
use eframe::egui_wgpu::{self, wgpu};
use egui::{TextureHandle, TextureId, Vec2};
use std::sync::Arc;

pub struct RemoteScreen {
    gpu: Option<GpuConverter>,
    /// The CPU fallback's texture.
    cpu: Option<TextureHandle>,
    /// What to draw, and its size in pixels. `None` until the first frame of
    /// a session.
    current: Option<(TextureId, Vec2)>,
}

impl RemoteScreen {
    pub fn new(render_state: Option<&egui_wgpu::RenderState>) -> Self {
        let gpu = render_state.map(|rs| GpuConverter::new(rs.device.clone(), rs.queue.clone(), rs.renderer.clone()));
        if gpu.is_none() {
            tracing::info!("No wgpu renderer; converting remote frames to RGBA on the CPU");
        }
        Self { gpu, cpu: None, current: None }
    }

    /// The texture holding the latest frame, and its size in pixels.
    pub fn texture(&self) -> Option<(TextureId, Vec2)> {
        self.current
    }

    /// Forgets the last session's picture (the textures are kept for reuse).
    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn upload(&mut self, ctx: &egui::Context, frame: &Nv12Frame) {
        let size = Vec2::new(frame.width as f32, frame.height as f32);
        let id = match &mut self.gpu {
            Some(gpu) => gpu.upload(frame),
            None => {
                let (w, h, rgba) = crate::capture::nv12_preview_rgba(
                    &frame.y, &frame.uv, frame.width, frame.height, frame.width, frame.height,
                );
                let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                match &mut self.cpu {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.cpu = Some(ctx.load_texture("remote_screen", image, egui::TextureOptions::LINEAR));
                    }
                }
                self.cpu.as_ref().map(TextureHandle::id).expect("just set")
            }
        };
        self.current = Some((id, size));
    }
}

/// Owns the wgpu side: the conversion pipeline, and the textures for the
/// current frame size.
struct GpuConverter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Arc<egui::mutex::RwLock<egui_wgpu::Renderer>>,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Registered with egui once, then pointed at each new RGB texture.
    texture_id: Option<TextureId>,
    target: Option<Target>,
}

/// Textures for one frame size.
struct Target {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    #[cfg_attr(not(test), allow(dead_code))]
    rgb: wgpu::Texture,
    rgb_view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// egui's own textures are `Rgba8Unorm` holding gamma-encoded colour, and
/// its shader treats what it samples that way; the converted frame matches.
const RGB_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl GpuConverter {
    fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        renderer: Arc<egui::mutex::RwLock<egui_wgpu::Renderer>>,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12_to_rgb"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12_to_rgb"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12_to_rgb"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12_to_rgb"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: RGB_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        // Only chroma is sampled between texels (it's half size); luma is
        // read texel for texel.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12_chroma"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            device,
            queue,
            renderer,
            pipeline,
            bind_group_layout,
            sampler,
            texture_id: None,
            target: None,
        }
    }

    fn upload(&mut self, frame: &Nv12Frame) -> TextureId {
        let (w, h) = (frame.width, frame.height);
        if self.target.as_ref().is_none_or(|t| (t.width, t.height) != (w, h)) {
            self.resize(w, h);
        }
        let target = self.target.as_ref().expect("resized above");

        write_plane(&self.queue, &target.y, &frame.y, w, w, h);
        write_plane(&self.queue, &target.uv, &frame.uv, w, w / 2, h / 2);

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("nv12_to_rgb"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12_to_rgb"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.rgb_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &target.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);

        self.texture_id.expect("registered in resize")
    }

    fn resize(&mut self, width: u32, height: u32) {
        let texture = |label, format, w, h, usage| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let plane_usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let y = texture("nv12_y", wgpu::TextureFormat::R8Unorm, width, height, plane_usage);
        let uv = texture("nv12_uv", wgpu::TextureFormat::Rg8Unorm, width / 2, height / 2, plane_usage);
        let rgb = texture(
            "remote_screen",
            RGB_FORMAT,
            width,
            height,
            // COPY_SRC only so the tests can read it back; it costs nothing.
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );

        let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
        let rgb_view = rgb.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12_to_rgb"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&y_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&uv_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });

        let mut renderer = self.renderer.write();
        match self.texture_id {
            Some(id) => renderer.update_egui_texture_from_wgpu_texture(
                &self.device,
                &rgb_view,
                wgpu::FilterMode::Linear,
                id,
            ),
            None => {
                self.texture_id =
                    Some(renderer.register_native_texture(&self.device, &rgb_view, wgpu::FilterMode::Linear));
            }
        }
        drop(renderer);

        self.target = Some(Target { width, height, y, uv, rgb, rgb_view, bind_group });
    }
}

impl Drop for GpuConverter {
    fn drop(&mut self) {
        if let Some(id) = self.texture_id {
            self.renderer.write().free_texture(&id);
        }
    }
}

/// Writes one tightly packed plane (`row_bytes` per row) into `texture`.
fn write_plane(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], row_bytes: u32, width: u32, height: u32) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row_bytes), rows_per_image: Some(height) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}

/// One triangle covering the target; each pixel reads its luma texel and
/// bilinearly upsampled chroma, then converts BT.709 studio range — what the
/// sender's GPU video processor writes and the encoders signal in the VUI —
/// to gamma-encoded RGB.
const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var chroma_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let luma = textureLoad(y_tex, vec2<i32>(in.pos.xy), 0).r;
    let chroma = textureSample(uv_tex, chroma_sampler, in.uv).rg;
    let y = (luma * 255.0 - 16.0) / 219.0;
    let cb = (chroma.x * 255.0 - 128.0) / 224.0;
    let cr = (chroma.y * 255.0 - 128.0) / 224.0;
    let rgb = vec3<f32>(
        y + 1.5748 * cr,
        y - 0.1873 * cb - 0.4681 * cr,
        y + 1.8556 * cb,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Converts a 4x2 frame, white on the left and pure red on the right
    /// (BT.709 studio range), on a real GPU and reads the result back.
    /// Skipped, with a note, on a machine with no wgpu adapter at all.
    #[test]
    fn converts_bt709_studio_range_nv12_to_rgb() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            println!("no wgpu adapter; skipping");
            return;
        };
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device");
        let renderer = egui_wgpu::Renderer::new(&device, RGB_FORMAT, egui_wgpu::RendererOptions::default());
        let mut converter = GpuConverter::new(device.clone(), queue.clone(), Arc::new(egui::mutex::RwLock::new(renderer)));

        // Two 2x2 chroma blocks: white (Y 235, neutral chroma) and red
        // (Y 63, Cb 102, Cr 240).
        let frame = Nv12Frame {
            width: 4,
            height: 2,
            y: vec![235, 235, 63, 63, 235, 235, 63, 63],
            uv: vec![128, 128, 102, 240],
        };
        converter.upload(&frame);

        let texture = &converter.target.as_ref().unwrap().rgb;
        let row_bytes = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row_bytes * 2) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(2),
                },
            },
            wgpu::Extent3d { width: 4, height: 2, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let data = buffer.slice(..).get_mapped_range().expect("mapped").to_vec();

        let pixel = |x: usize, y: usize| {
            let i = y * row_bytes as usize + x * 4;
            [data[i], data[i + 1], data[i + 2], data[i + 3]]
        };
        let close = |got: [u8; 4], want: [u8; 4]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 3);
        // The outer columns sit on their own chroma sample; the inner ones
        // are blended with the neighbour's.
        for y in 0..2 {
            assert!(close(pixel(0, y), [255, 255, 255, 255]), "white came out {:?}", pixel(0, y));
            assert!(close(pixel(3, y), [255, 0, 0, 255]), "red came out {:?}", pixel(3, y));
        }
    }
}

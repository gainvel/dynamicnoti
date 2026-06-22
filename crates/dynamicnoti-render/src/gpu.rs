//! wgpu device/queue plus the two draw pipelines the island needs:
//! a rounded-rect SDF pipeline (background pill, progress bars, media-icon shapes) and a
//! textured-quad pipeline (album art / images). Both share a unit-quad vertex shader that
//! turns a pixel-space rect into a triangle strip; everything is instanced.
//!
//! Colors are premultiplied-alpha in LINEAR space — we pick an sRGB surface format so the
//! hardware does the linear→sRGB encode on write, and glyphon (also told the sRGB format)
//! matches. Blend is `One, OneMinusSrcAlpha` (premultiplied over).

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// Screen-size uniform shared by both pipelines (pixel→NDC conversion lives in the shader).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen: [f32; 2],
    _pad: [f32; 2],
}

/// One rounded rectangle. `meta = [corner_radius, _, _, _]`. `color` is premultiplied linear.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct RectInstance {
    pub rect: [f32; 4],
    pub color: [f32; 4],
    pub meta: [f32; 4],
}

/// One textured rounded rectangle. `meta = [corner_radius, opacity, _, _]`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ImageInstance {
    pub rect: [f32; 4],
    pub meta: [f32; 4],
}

/// Owns the GPU context. Created once; survives for the whole process.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Build the instance and pick a LOW-POWER adapter (the iGPU on a multi-GPU box — this is a
    /// tiny always-on overlay). Adapter selection without a surface; later layer surfaces from
    /// the same Wayland connection are compatible. `WGPU_BACKEND` overrides the backend choice.
    pub fn new() -> anyhow::Result<Gpu> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| anyhow::anyhow!("no suitable wgpu adapter"))?;
        let info = adapter.get_info();
        tracing::info!(target: "render", "wgpu adapter: {} ({:?}, {:?})", info.name, info.device_type, info.backend);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor { label: Some("dynamicnoti-device"), ..Default::default() },
            None,
        ))?;
        Ok(Gpu { instance, adapter, device, queue })
    }

    /// Choose the surface format (prefer sRGB so glyphon + the linear pipeline match) and the
    /// best translucent composite-alpha mode (premultiplied if KWin offers it).
    pub fn pick_config(
        &self,
        surface: &wgpu::Surface<'static>,
    ) -> (wgpu::TextureFormat, wgpu::CompositeAlphaMode) {
        let caps = surface.get_capabilities(&self.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let alpha = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            wgpu::CompositeAlphaMode::PostMultiplied
        } else {
            caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        (format, alpha)
    }
}

/// Pipelines + shared resources, built once the surface format is known.
pub struct Pipelines {
    rect_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    image_bg_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Pipelines {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Pipelines {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("island-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals-bg"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        let image_bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("island-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let targets = [Some(wgpu::ColorTargetState {
            format,
            blend: Some(blend),
            write_mask: wgpu::ColorWrites::ALL,
        })];

        let rect_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rect-pl"),
            bind_group_layouts: &[&globals_layout],
            push_constant_ranges: &[],
        });
        let image_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image-pl"),
            bind_group_layouts: &[&globals_layout, &image_bg_layout],
            push_constant_ranges: &[],
        });

        let rect_attrs =
            wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        let rect_buffers = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<RectInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &rect_attrs,
        }];
        let image_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
        let image_buffers = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ImageInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &image_attrs,
        }];

        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        };

        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect-pipeline"),
            layout: Some(&rect_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_rect"),
                compilation_options: Default::default(),
                buffers: &rect_buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_rect"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            primitive,
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image-pipeline"),
            layout: Some(&image_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_image"),
                compilation_options: Default::default(),
                buffers: &image_buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_image"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            primitive,
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Pipelines {
            rect_pipeline,
            image_pipeline,
            globals_buf,
            globals_bg,
            image_bg_layout,
            sampler,
        }
    }

    pub fn image_bg_layout(&self) -> &wgpu::BindGroupLayout {
        &self.image_bg_layout
    }
    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// Push the current surface pixel size into the globals uniform.
    pub fn set_screen(&self, queue: &wgpu::Queue, w: f32, h: f32) {
        let g = Globals { screen: [w, h], _pad: [0.0, 0.0] };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));
    }

    /// Record the rounded-rect instances into `pass`. Caller supplies an already-built buffer.
    pub fn draw_rects<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, buf: &'a wgpu::Buffer, count: u32) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.rect_pipeline);
        pass.set_bind_group(0, &self.globals_bg, &[]);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..4, 0..count);
    }

    /// Record one textured rect (`buf` holds a single ImageInstance) with its bind group.
    pub fn draw_image<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        buf: &'a wgpu::Buffer,
        bind_group: &'a wgpu::BindGroup,
    ) {
        pass.set_pipeline(&self.image_pipeline);
        pass.set_bind_group(0, &self.globals_bg, &[]);
        pass.set_bind_group(1, bind_group, &[]);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..4, 0..1);
    }
}

/// Build a GPU instance buffer from a slice of POD instances.
pub fn instance_buffer<T: Pod>(device: &wgpu::Device, data: &[T]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("instances"),
        contents: bytemuck::cast_slice(data),
        usage: wgpu::BufferUsages::VERTEX,
    })
}

/// sRGB-encoded 8-bit channel → linear float, then premultiply by `a`. Returns linear
/// premultiplied RGBA suitable for the `One, OneMinusSrcAlpha` blend.
pub fn premul_linear(r: u8, g: u8, b: u8, a: u8) -> [f32; 4] {
    let s2l = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let af = a as f32 / 255.0;
    [s2l(r) * af, s2l(g) * af, s2l(b) * af, af]
}

const SHADER: &str = r#"
struct Globals { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> g: Globals;

fn corner(vi: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 4>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 1.0));
    return c[vi];
}
fn to_ndc(px: vec2<f32>) -> vec4<f32> {
    return vec4(px.x / g.screen.x * 2.0 - 1.0, 1.0 - px.y / g.screen.y * 2.0, 0.0, 1.0);
}
// Signed distance to a rounded box centred at origin, half-extents h, radius r.
fn sd_round_box(p: vec2<f32>, h: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - (h - vec2(r));
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Distance to a right-pointing triangle inscribed in the box (half-extents h). Convex-polygon
// SDF via outward edge normals — approximate near vertices but crisp enough for a tiny glyph.
fn sd_play(p: vec2<f32>, h: vec2<f32>) -> f32 {
    let a = vec2(-h.x, -h.y);
    let b = vec2(h.x, 0.0);
    let c = vec2(-h.x, h.y);
    let cen = (a + b + c) / 3.0;
    var d = -1e9;
    var v0 = a; var v1 = b;
    for (var i = 0; i < 3; i = i + 1) {
        if (i == 1) { v0 = b; v1 = c; }
        if (i == 2) { v0 = c; v1 = a; }
        let e = v1 - v0;
        var n = normalize(vec2(-e.y, e.x));
        let mid = (v0 + v1) * 0.5;
        if (dot(n, mid - cen) < 0.0) { n = -n; }
        d = max(d, dot(p - v0, n));
    }
    return d;
}

struct RectVs {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) hext: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) radius: f32,
    @location(4) shape: f32,
};
@vertex
fn vs_rect(@builtin(vertex_index) vi: u32,
           @location(0) rect: vec4<f32>,
           @location(1) color: vec4<f32>,
           @location(2) params: vec4<f32>) -> RectVs {
    let c = corner(vi);
    let px = rect.xy + c * rect.zw;
    var out: RectVs;
    out.pos = to_ndc(px);
    out.local = (c - vec2(0.5)) * rect.zw;
    out.hext = rect.zw * 0.5;
    out.color = color;
    out.radius = min(params.x, min(rect.z, rect.w) * 0.5);
    out.shape = params.y;
    return out;
}
@fragment
fn fs_rect(in: RectVs) -> @location(0) vec4<f32> {
    var d: f32;
    if (in.shape > 0.5) {
        d = sd_play(in.local, in.hext);
    } else {
        d = sd_round_box(in.local, in.hext, in.radius);
    }
    let a = 1.0 - smoothstep(-0.7, 0.7, d);
    return in.color * a;
}

struct ImageVs {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) hext: vec2<f32>,
    @location(3) radius: f32,
    @location(4) opacity: f32,
};
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
@vertex
fn vs_image(@builtin(vertex_index) vi: u32,
            @location(0) rect: vec4<f32>,
            @location(1) params: vec4<f32>) -> ImageVs {
    let c = corner(vi);
    let px = rect.xy + c * rect.zw;
    var out: ImageVs;
    out.pos = to_ndc(px);
    out.uv = c;
    out.local = (c - vec2(0.5)) * rect.zw;
    out.hext = rect.zw * 0.5;
    out.radius = min(params.x, min(rect.z, rect.w) * 0.5);
    out.opacity = params.y;
    return out;
}
@fragment
fn fs_image(in: ImageVs) -> @location(0) vec4<f32> {
    let texel = textureSample(tex, samp, in.uv);
    let d = sd_round_box(in.local, in.hext, in.radius);
    let cov = 1.0 - smoothstep(-0.7, 0.7, d);
    let a = texel.a * cov * in.opacity;
    return vec4(texel.rgb * a, a);
}
"#;

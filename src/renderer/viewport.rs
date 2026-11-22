//! Interactive rendering. Geometry and the last drawn frame stay on the device.
mod detail;
mod scene;
#[cfg(test)]
mod tests;
use super::{Projection, RenderOptions, bed_lines, glyph};
use crate::{
    colors::{self, BACKGROUND},
    types::{ColorMode, ExtrusionType, ToolpathData},
};
use eframe::{
    egui,
    egui_wgpu::RenderState,
    wgpu::{self, util::DeviceExt},
};
use scene::{Scene, Stroke};
use std::sync::Arc;

const COLOR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

struct Target {
    size: [u32; 2],
    color: wgpu::TextureView,
    image: wgpu::Texture,
    sampled_copy: Option<wgpu::Texture>,
    depth: wgpu::TextureView,
    id: egui::TextureId,
}

pub(crate) struct Model {
    pub data: Arc<ToolpathData>,
    pub legends: colors::Legends,
    scene: Scene,
    bed: wgpu::Buffer,
    bed_lines: u32,
}

impl Model {
    pub fn new(device: &wgpu::Device, data: ToolpathData) -> Self {
        let (ranges, legends) = colors::Legends::collect(&data);
        let scene = Scene::upload(device, &data, &ranges);
        let mut strokes = Vec::new();
        let labels = bed_lines(
            [data.bed_x, data.bed_y],
            |[a, b], [r, g, bl], w, opacity| {
                let style = [
                    r,
                    g,
                    bl,
                    (opacity * 255.).round() as u8,
                    0,
                    0,
                    (w * 10.).round() as u8,
                    2,
                ];
                strokes.push(Stroke::new(a, b, scene.origin, style));
            },
        );
        for (p, label, [r, g, b]) in labels {
            for (y, bits) in glyph(label.chars().next().unwrap()).into_iter().enumerate() {
                for x in 0..5 {
                    if bits & (1 << (4 - x)) != 0 {
                        strokes.push(Stroke::new(
                            p,
                            p,
                            scene.origin,
                            [r, g, b, 255, x, y as u8, 10, 3],
                        ));
                    }
                }
            }
        }
        let bed = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("print bed"),
            contents: bytemuck::cast_slice(&strokes),
            usage: wgpu::BufferUsages::VERTEX,
        });
        Self {
            data: Arc::new(data),
            legends,
            scene,
            bed,
            bed_lines: strokes.len() as u32,
        }
    }
}

pub(crate) struct Viewport {
    #[cfg(test)]
    drawn_strokes: u32,
    pub state: RenderState,
    uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    plate: wgpu::RenderPipeline,
    extrusion: wgpu::RenderPipeline,
    travel: wgpu::RenderPipeline,
    target: Option<Target>,
}

impl Viewport {
    pub fn new(state: RenderState) -> Self {
        let device = &state.device;
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera"),
            size: 128,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut palette = [[0u32; 256]; 6];
        for mode in ColorMode::ALL {
            for (i, pixel) in palette[mode as usize].iter_mut().enumerate() {
                let [r, g, b] = if mode == ColorMode::Feature {
                    colors::feature_color(ExtrusionType::ALL[i.min(13)])
                } else {
                    colors::gradient_color(mode, i as f64 / 255.)
                };
                *pixel = u32::from_le_bytes([r, g, b, 255]);
            }
        }
        let palette = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("toolpath palettes"),
            contents: bytemuck::cast_slice(&palette),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewport bindings"),
            entries: &[0, 1].map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }),
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: palette.as_entire_binding(),
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::include_wgsl!("viewport/strokes.wgsl"));
        let pipeline = |vertex, fragment, depth_write| {
            let buffers = [wgpu::VertexBufferLayout {
                array_stride: size_of::<Stroke>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![0=>Float32x3,1=>Float32x3,2=>Uint8x4,3=>Uint8x4],
            }];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(vertex),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vertex),
                    compilation_options: Default::default(),
                    buffers: if vertex == "plate" { &[] } else { &buffers },
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH,
                    depth_write_enabled: depth_write,
                    depth_compare: wgpu::CompareFunction::GreaterEqual,
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview: None,
                cache: None,
            })
        };
        let plate = pipeline("plate", "plate_color", false);
        let extrusion = pipeline("stroke", "ink", true);
        let travel = pipeline("stroke", "ink", false);
        Self {
            #[cfg(test)]
            drawn_strokes: 0,
            state,
            uniform,
            bind,
            plate,
            extrusion,
            travel,
            target: None,
        }
    }

    fn resize(&mut self, size: [u32; 2]) {
        let device = &self.state.device;
        let alias = self
            .state
            .adapter
            .get_downlevel_capabilities()
            .flags
            .contains(wgpu::DownlevelFlags::VIEW_FORMATS);
        let texture = |format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("toolpath viewport"),
                size: wgpu::Extent3d {
                    width: size[0],
                    height: size[1],
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: if format == COLOR && alias {
                    &[wgpu::TextureFormat::Rgba8UnormSrgb]
                } else {
                    &[]
                },
            })
        };
        let copy = if cfg!(test) || !alias {
            wgpu::TextureUsages::COPY_SRC
        } else {
            wgpu::TextureUsages::empty()
        };
        let image = texture(
            COLOR,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | copy,
        );
        let color = image.create_view(&Default::default());
        // Blend coverage in the same color space as PNG export. egui samples
        // sRGB textures, so expose those same bytes with an sRGB sampling view.
        // OpenGL cannot reinterpret a texture view. Copy the same bytes on
        // the device only when this backend needs a separate sampling texture.
        let sampled_copy = (!alias).then(|| {
            texture(
                COLOR.add_srgb_suffix(),
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | copy,
            )
        });
        let sampled =
            sampled_copy
                .as_ref()
                .unwrap_or(&image)
                .create_view(&wgpu::TextureViewDescriptor {
                    format: Some(COLOR.add_srgb_suffix()),
                    ..Default::default()
                });
        let depth =
            texture(DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default());
        let mut renderer = self.state.renderer.write();
        let id = if let Some(old) = &self.target {
            renderer.update_egui_texture_from_wgpu_texture(
                device,
                &sampled,
                wgpu::FilterMode::Linear,
                old.id,
            );
            old.id
        } else {
            renderer.register_native_texture(device, &sampled, wgpu::FilterMode::Linear)
        };
        self.target = Some(Target {
            size,
            color,
            image,
            sampled_copy,
            depth,
            id,
        });
    }

    pub fn image(
        &mut self,
        model: &Model,
        o: &RenderOptions,
        dirty: bool,
        moving: bool,
    ) -> Result<egui::TextureId, super::RenderError> {
        let size = [o.width, o.height];
        if self.target.as_ref().is_none_or(|t| t.size != size) {
            self.resize(size);
        } else if !dirty {
            return Ok(self.target.as_ref().unwrap().id);
        }
        let data = &model.data;
        let layers = o.layers(data.total_layers());
        let selection = model.scene.selection(data, layers.clone(), o.show_travel);
        let bed = [data.bed_x, data.bed_y];
        let p = Projection::framed(
            selection.as_ref().map(|s| s.bounds()),
            bed,
            o,
            None,
            |p, screen| {
                if let Some(subject) = selection {
                    subject.project(p, screen);
                }
            },
        )?;
        let vec4 = |v: [f64; 3], w: f64| [v[0] as f32, v[1] as f32, v[2] as f32, w as f32];
        // Reversed floating-point depth preserves precision in perspective even
        // when a distant purge line makes the visible depth range unusually large.
        let mut depth_bounds = data.bounds;
        if o.show_bed {
            depth_bounds.include([0.; 3]);
            depth_bounds.include([bed[0], bed[1], 0.]);
        }
        let far = p.distance
            + (0..3)
                .map(|i| {
                    (depth_bounds.max[i] - p.center[i])
                        .abs()
                        .max((depth_bounds.min[i] - p.center[i]).abs())
                })
                .sum::<f64>()
            + 1.;
        let error = if moving {
            // arc_tolerance bounds a quarter pixel. Allow two pixels while
            // moving, then restore every original stroke when the camera stops.
            (8. * super::features::arc_tolerance(&p, model.scene.object_bounds)) as f32
        } else {
            0.
        };
        let uniforms = [
            vec4(p.basis[0], p.scale),
            vec4(p.basis[1], p.near),
            vec4(p.basis[2], 1. / far),
            vec4(super::subtract(p.center, model.scene.origin), p.distance),
            [
                o.width as f32,
                o.height as f32,
                p.screen_center[0] as f32,
                p.screen_center[1] as f32,
            ],
            [
                o.line_width as f32,
                o.color_mode as u8 as f32,
                u8::from(p.perspective) as f32,
                0.,
            ],
            [bed[0] as f32, bed[1] as f32, 0., 0.],
            vec4(model.scene.origin, 0.),
        ];
        self.state
            .queue
            .write_buffer(&self.uniform, 0, bytemuck::cast_slice(&uniforms));
        #[cfg(test)]
        {
            self.drawn_strokes = 0;
        }
        let target = self.target.as_ref().unwrap();
        let mut enc = self
            .state
            .device
            .create_command_encoder(&Default::default());
        {
            let [r, g, b] = BACKGROUND.map(|v| f64::from(v) / 255.);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toolpaths"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.color,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1. }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_bind_group(0, &self.bind, &[]);
            if o.show_bed {
                pass.set_pipeline(&self.plate);
                pass.draw(0..4, 0..1);
                pass.set_pipeline(&self.extrusion);
                pass.set_vertex_buffer(0, model.bed.slice(..));
                pass.draw(0..4, 0..model.bed_lines);
            }
            for (i, pipeline) in [&self.extrusion, &self.travel]
                .into_iter()
                .take(if o.show_travel { 2 } else { 1 })
                .enumerate()
            {
                pass.set_pipeline(pipeline);
                for (batch, range) in model.scene.draws(layers.clone(), i) {
                    batch.draw(range, error, |buffer, range| {
                        #[cfg(test)]
                        {
                            self.drawn_strokes += range.end - range.start;
                        }
                        pass.set_vertex_buffer(0, buffer.slice(..));
                        pass.draw(0..4, range);
                    });
                }
            }
        }
        if let Some(copy) = &target.sampled_copy {
            enc.copy_texture_to_texture(
                target.image.as_image_copy(),
                copy.as_image_copy(),
                target.image.size(),
            );
        }
        self.state.queue.submit([enc.finish()]);
        Ok(target.id)
    }
}

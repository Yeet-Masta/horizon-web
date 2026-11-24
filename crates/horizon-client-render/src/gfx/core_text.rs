//! A resource pack's `core/text` on world text, as vanilla 26.3's `TEXT_POLYGON_OFFSET` and
//! `TEXT_SEE_THROUGH` draw text displays, name tags and their backgrounds.

use crate::core_shader::{CoreProgram, Glue};
use crate::font::{FontPages, PageKind, nearest_repeat};
use std::sync::Arc;

/// The entity vertices' light bytes are sky then block; vanilla's `UV2` is block then sky.
pub(crate) const WORLD_GLUE: Glue = Glue {
    inputs: &[
        ("Position", "vec3"),
        ("Color", "vec4"),
        ("UV0", "vec2"),
        ("UV2", "ivec2"),
    ],
    prelude: "layout(location = 0) in vec3 hz_pos;\n\
              layout(location = 6) in vec2 hz_uv;\n\
              layout(location = 3) in vec4 hz_color;\n\
              layout(location = 4) in uvec4 hz_light;\n\
              int hz_text_vertex_id() { int i = int(gl_VertexIndex); return i / 4 * 4 + (i + 1) % 4; }",
    setup: "Position = hz_pos; Color = hz_color; UV0 = hz_uv; UV2 = ivec2(hz_light.y, hz_light.x);",
    finish: "",
    vertex_id: "hz_text_vertex_id()",
};

pub(crate) const SEE_THROUGH_GLUE: Glue = Glue {
    inputs: &[("Position", "vec3"), ("Color", "vec4"), ("UV0", "vec2")],
    prelude: "layout(location = 0) in vec3 hz_pos;\n\
              layout(location = 6) in vec2 hz_uv;\n\
              layout(location = 3) in vec4 hz_color;\n\
              int hz_text_vertex_id() { int i = int(gl_VertexIndex); return i / 4 * 4 + (i + 1) % 4; }",
    setup: "Position = hz_pos; Color = hz_color; UV0 = hz_uv;",
    finish: "",
    vertex_id: "hz_text_vertex_id()",
};

#[derive(Default)]
pub struct TextShaders {
    pub gui: Option<Arc<CoreProgram>>,
    pub world: Option<Arc<CoreProgram>>,
    pub see_through: Option<Arc<CoreProgram>>,
    pub(crate) gui_grayscale: Option<Arc<CoreProgram>>,
    pub(crate) world_grayscale: Option<Arc<CoreProgram>>,
    pub(crate) see_through_grayscale: Option<Arc<CoreProgram>>,
    pub(crate) pages: Option<FontPages>,
}

impl TextShaders {
    pub(crate) fn has_gui(&self) -> bool {
        self.gui.is_some() || self.gui_grayscale.is_some()
    }

    pub(crate) fn has_world(&self) -> bool {
        self.world.is_some()
            || self.world_grayscale.is_some()
            || self.see_through.is_some()
            || self.see_through_grayscale.is_some()
    }

    pub(crate) fn any(&self) -> bool {
        self.has_gui() || self.has_world()
    }

    /// Name tags use `TEXT` for the visible copy, not `TEXT_POLYGON_OFFSET`.
    pub fn has_normal(&self) -> bool {
        self.world.is_some() || self.world_grayscale.is_some()
    }
}

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Normal,
    PolygonOffset,
    SeeThrough,
}

#[derive(Clone, Copy)]
pub(super) struct PackBlocks {
    pub(super) globals: [u8; 48],
    pub(super) fog: [f32; 6],
}

impl super::Renderer {
    pub fn set_world_text_shaders(&mut self, shaders: &TextShaders) {
        self.core_text = CoreText::new(&self.device, self.output.frame.format, shaders);
    }

    pub(super) fn pack_blocks(
        &self,
        frame: &super::Frame,
        fog: [f32; 4],
        ends: [f32; 4],
    ) -> PackBlocks {
        let globals = crate::post_chain::Globals {
            glint_alpha: frame.glint_alpha,
            ..frame.post_effects.globals
        };
        let size = [self.config.width, self.config.height];
        PackBlocks {
            globals: globals.bytes(size, frame.post_effects.menu_blur_radius as i32),
            fog: [fog[0], fog[1], fog[2], fog[3], ends[0], ends[1]],
        }
    }

    pub(super) fn write_core_text(
        &self,
        pack: &PackBlocks,
        view: &super::View,
        pose: glam::Mat4,
        jitter: glam::Mat4,
        fog_color: [f32; 4],
    ) {
        let Some(core) = &self.core_text else {
            return;
        };
        let data =
            super::core_lines::uniforms(pack.globals, view, pose, jitter, 1.0, fog_color, pack.fog);
        self.queue.write_buffer(&core.uniform, 0, &data);
    }

    pub(super) fn prepare_world_text(&mut self) {
        if let Some(core) = &mut self.core_text {
            core.prepare(&self.device, (&self.lightmap_view, &self.lightmap_sampler));
        }
    }

    pub(super) fn draw_world_text(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        mode: Mode,
        native: &wgpu::RenderPipeline,
        first: u32,
        count: u32,
        posed: u32,
    ) {
        let draw_native = |pass: &mut wgpu::RenderPass<'_>, first: u32, count: u32| {
            pass.set_vertex_buffer(0, self.entity_buffer.slice(..));
            pass.set_pipeline(native);
            pass.set_bind_group(0, &self.entity_group, &[]);
            pass.draw_indexed(0..count * 6, (first * 4) as i32, 0..1);
        };
        if let Some(core) = &self.core_text {
            let (start, end) = (first * 4 - posed, (first + count) * 4 - posed);
            let vertices = &self.entity_batch.combined[start as usize..end as usize];
            core.draw_runs(mode, vertices, |range, pack| {
                let first = first + range.start;
                let count = range.end - range.start;
                if let Some((pipeline, group)) = pack {
                    CoreText::draw_bound(
                        pass,
                        &self.entity_buffer,
                        (pipeline, group),
                        first..first + count,
                    );
                } else {
                    draw_native(pass, first, count);
                }
            });
            pass.set_vertex_buffer(0, self.entity_buffer.slice(..));
        } else {
            draw_native(pass, first, count);
        }
    }
}

const BLOCK_OFFSETS: [u64; 4] = [0, 256, 512, 768];
const BLOCK_SIZES: [u64; 4] = [48, 64, 160, 48];

pub(super) struct CoreText {
    normal: [Option<wgpu::RenderPipeline>; 2],
    polygon_offset: [Option<wgpu::RenderPipeline>; 2],
    see_through: [Option<wgpu::RenderPipeline>; 2],
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pages: Option<FontPages>,
    page_generation: u64,
    groups: Vec<(PageKind, wgpu::BindGroup)>,
    uniform: wgpu::Buffer,
}

impl CoreText {
    pub(super) fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        shaders: &TextShaders,
    ) -> Option<Self> {
        if !shaders.has_world() {
            return None;
        }
        let buffer = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(BLOCK_SIZES[binding as usize]),
            },
            count: None,
        };
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Resource pack world text"),
            entries: &[
                buffer(0),
                buffer(1),
                buffer(2),
                buffer(3),
                texture(4),
                sampler(5),
                texture(8),
                sampler(9),
            ],
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Resource pack world text uniforms"),
            size: 1024,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = nearest_repeat(device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Resource pack world text"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let attributes = wgpu::vertex_attr_array![0=>Float32x3,1=>Float32x2,2=>Uint32,3=>Unorm8x4,4=>Uint8x4,5=>Snorm8x4,6=>Float32x2];
        let buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<crate::mesher::Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        })];
        let pipeline = |label, program: &CoreProgram, depth: wgpu::DepthStencilState, blend| {
            let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
            let module = |module: &naga::Module| {
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(label),
                    source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module.clone())),
                })
            };
            let (vertex, fragment) = (module(&program.vertex), module(&program.fragment));
            let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &vertex,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    buffers: &buffers,
                },
                primitive: wgpu::PrimitiveState {
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: Some(depth),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &fragment,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            let internal = pollster::block_on(internal.pop());
            let validation = pollster::block_on(validation.pop());
            if let Some(e) = internal.or(validation) {
                log::warn!("{label} is not used: {e}");
                None
            } else {
                Some(pipe)
            }
        };
        let depth = |write, compare, bias| wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(write),
            depth_compare: Some(compare),
            stencil: Default::default(),
            bias,
        };
        let offset = wgpu::DepthBiasState {
            constant: 10,
            slope_scale: 1.0,
            clamp: 0.0,
        };
        let alpha = Some(wgpu::BlendState::ALPHA_BLENDING);
        let variants = [shaders.world.as_deref(), shaders.world_grayscale.as_deref()];
        let normal = variants.map(|program| {
            pipeline(
                "Resource pack normal text",
                program?,
                depth(
                    true,
                    wgpu::CompareFunction::GreaterEqual,
                    Default::default(),
                ),
                alpha,
            )
        });
        let polygon_offset = variants.map(|program| {
            pipeline(
                "Resource pack polygon offset text",
                program?,
                depth(true, wgpu::CompareFunction::GreaterEqual, offset),
                alpha,
            )
        });
        let see_through = [
            shaders.see_through.as_deref().and_then(|program| {
                pipeline(
                    "Resource pack see-through text",
                    program,
                    depth(false, wgpu::CompareFunction::Always, Default::default()),
                    alpha,
                )
            }),
            shaders
                .see_through_grayscale
                .as_deref()
                .and_then(|program| {
                    pipeline(
                        "Resource pack see-through grayscale text",
                        program,
                        depth(false, wgpu::CompareFunction::Always, Default::default()),
                        None,
                    )
                }),
        ];
        if normal
            .iter()
            .chain(&polygon_offset)
            .chain(&see_through)
            .all(Option::is_none)
        {
            return None;
        }
        Some(Self {
            normal,
            polygon_offset,
            see_through,
            layout,
            sampler,
            pages: shaders.pages.clone(),
            page_generation: 0,
            groups: Vec::new(),
            uniform,
        })
    }

    pub(super) fn has_polygon_offset(&self) -> bool {
        self.polygon_offset.iter().any(Option::is_some)
    }

    fn prepare(&mut self, device: &wgpu::Device, lightmap: (&wgpu::TextureView, &wgpu::Sampler)) {
        let Some(pages) = &self.pages else { return };
        let generation = pages.generation();
        if self.page_generation != generation {
            self.groups.clear();
            self.page_generation = generation;
        }
        let groups = &mut self.groups;
        pages.for_each_view(groups.len(), |kind, view| {
            groups.push((
                kind,
                Self::group(
                    device,
                    &self.layout,
                    &self.uniform,
                    (view, &self.sampler),
                    lightmap,
                ),
            ));
        });
    }

    fn group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform: &wgpu::Buffer,
        glyphs: (&wgpu::TextureView, &wgpu::Sampler),
        lightmap: (&wgpu::TextureView, &wgpu::Sampler),
    ) -> wgpu::BindGroup {
        let mut resources: Vec<wgpu::BindGroupEntry> = (0..4)
            .map(|i| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: uniform,
                    offset: BLOCK_OFFSETS[i],
                    size: wgpu::BufferSize::new(BLOCK_SIZES[i]),
                }),
            })
            .collect();
        resources.extend([
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(glyphs.0),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::Sampler(glyphs.1),
            },
            wgpu::BindGroupEntry {
                binding: 8,
                resource: wgpu::BindingResource::TextureView(lightmap.0),
            },
            wgpu::BindGroupEntry {
                binding: 9,
                resource: wgpu::BindingResource::Sampler(lightmap.1),
            },
        ]);
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Resource pack world text page"),
            layout,
            entries: &resources,
        })
    }

    fn draw_bound(
        pass: &mut wgpu::RenderPass<'_>,
        vertices: &wgpu::Buffer,
        (pipeline, group): (&wgpu::RenderPipeline, &wgpu::BindGroup),
        quads: std::ops::Range<u32>,
    ) {
        let offset =
            u64::from(quads.start) * 4 * std::mem::size_of::<crate::mesher::Vertex>() as u64;
        // Start gl_VertexID at the beginning of this texture's batch.
        pass.set_vertex_buffer(0, vertices.slice(offset..));
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, group, &[]);
        pass.draw_indexed(0..(quads.end - quads.start) * 6, 0, 0..1);
    }

    fn draw_runs(
        &self,
        mode: Mode,
        vertices: &[crate::mesher::Vertex],
        mut draw: impl FnMut(std::ops::Range<u32>, Option<(&wgpu::RenderPipeline, &wgpu::BindGroup)>),
    ) {
        let pipelines = match mode {
            Mode::Normal => &self.normal,
            Mode::PolygonOffset => &self.polygon_offset,
            Mode::SeeThrough => &self.see_through,
        };
        let mut quads = vertices
            .as_chunks::<4>()
            .0
            .iter()
            .map(|quad| {
                let vertex = &quad[0];
                let high = u16::from_le_bytes([vertex.normal[0] as u8, vertex.normal[1] as u8]);
                (vertex.light >> 16) | (u32::from(high) << 16)
            })
            .peekable();
        let mut start = 0;
        while let Some(page) = quads.next() {
            let mut end = start + 1;
            while quads.next_if_eq(&page).is_some() {
                end += 1;
            }
            let pack = page.checked_sub(1).and_then(|id| {
                let (kind, group) = self.groups.get(id as usize)?;
                let index = usize::from(*kind == PageKind::Grayscale);
                Some((pipelines[index].as_ref()?, group))
            });
            draw(start..end, pack);
            start = end;
        }
    }
}

#[cfg(test)]
mod tests;

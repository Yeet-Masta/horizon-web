use super::*;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = crate::gfx::test_gpu::instance();
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    pollster::block_on(adapter.request_device(&Default::default())).ok()
}

#[test]
fn a_pack_text_shader_reads_the_light_as_vanilla_does() {
    let _gpu = crate::gfx::test_gpu::lock();
    let Some((device, queue)) = device() else {
        eprintln!("no GPU: skipping");
        return;
    };
    let vsh = "#version 330\n#extension GL_ARB_separate_shader_objects : require\n\
        layout(location = 0) in vec3 Position;\nlayout(location = 1) in vec4 Color;\n\
        layout(location = 2) in vec2 UV0;\nlayout(location = 3) in ivec2 UV2;\n\
        layout(location = 0) flat out ivec2 light;\n\
        void main() { gl_Position = vec4(Position, 1.0); light = UV2; }\n";
    let fsh = "#version 330\n#extension GL_ARB_separate_shader_objects : require\n\
        layout(location = 0) flat in ivec2 light;\nlayout(location = 0) out vec4 fragColor;\n\
        void main() { fragColor = vec4(vec2(light) / 255.0, 0.0, 1.0); }\n";
    let program =
        crate::core_shader::translate_core(vsh, fsh, &[], &|_| None, &WORLD_GLUE).unwrap();
    const SIZE: u32 = 64;
    let texture = |format, usage| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let binding = wgpu::TextureUsages::TEXTURE_BINDING;
    let atlas = texture(wgpu::TextureFormat::Rgba8Unorm, binding).create_view(&Default::default());
    let sampler = device.create_sampler(&Default::default());
    let text = CoreText::new(
        &device,
        wgpu::TextureFormat::Rgba8Unorm,
        &TextShaders {
            world: Some(Arc::new(program)),
            ..Default::default()
        },
    )
    .expect("the pack's pipeline");
    let group = CoreText::group(
        &device,
        &text.layout,
        &text.uniform,
        (&atlas, &text.sampler),
        (&atlas, &sampler),
    );
    let target = wgpu::TextureUsages::RENDER_ATTACHMENT;
    let color = texture(
        wgpu::TextureFormat::Rgba8Unorm,
        target | wgpu::TextureUsages::COPY_SRC,
    );
    let depth = texture(wgpu::TextureFormat::Depth32Float, target);
    let light = 0x30 | (0xC0 << 8);
    let corner = |x: f32, y: f32| crate::mesher::Vertex {
        pos: [x, y, 0.5],
        uv: [0.0; 2],
        layer: 0,
        color: u32::MAX,
        light,
        normal: [0; 4],
        glint_uv: [0.0; 2],
    };
    let quad = [
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ];
    let vertices = wgpu::util::DeviceExt::create_buffer_init(
        &device,
        &wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&quad),
            usage: wgpu::BufferUsages::VERTEX,
        },
    );
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(SIZE * SIZE * 4),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let color_view = color.create_view(&Default::default());
        let depth_view = depth.create_view(&Default::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        pass.set_pipeline(text.polygon_offset[0].as_ref().unwrap());
        pass.set_bind_group(0, &group, &[]);
        pass.set_vertex_buffer(0, vertices.slice(..));
        pass.draw(0..6, 0..1);
    }
    encoder.copy_texture_to_buffer(
        color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 4),
                rows_per_image: Some(SIZE),
            },
        },
        color.size(),
    );
    queue.submit([encoder.finish()]);
    read.slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let pixels = read.slice(..).get_mapped_range().unwrap().to_vec();
    let middle = ((SIZE / 2 * SIZE + SIZE / 2) * 4) as usize;
    assert_eq!(
        &pixels[middle..middle + 4],
        &[0xC0, 0x30, 0, 0xFF],
        "UV2 is (block, sky)"
    );
}

#[test]
#[ignore = "daily"]
fn vanillas_text_shader_makes_both_world_text_pipelines() {
    let _gpu = crate::gfx::test_gpu::lock();
    let root = horizon_std::env::var_os("HORIZON_TEST_RESOURCES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.local/horizon-client")
                .join(horizon_core::VANILLA_VERSION)
        });
    let read = |path: &str| horizon_std::fs::read_to_string(root.join(path)).ok();
    let vertex = read("assets/minecraft/shaders/core/text.vsh").expect("vanilla's text.vsh");
    let fragment = read("assets/minecraft/shaders/core/text.fsh").expect("vanilla's text.fsh");
    let include = |id: &str| {
        let (namespace, path) = id.split_once(':')?;
        read(&format!("assets/{namespace}/{path}"))
    };
    let translate = |defines: &[&str], glue: &Glue| {
        crate::core_shader::translate_core(&vertex, &fragment, defines, &include, glue)
            .expect("vanilla's text shader translates")
    };
    let world = translate(&[], &WORLD_GLUE);
    let see_through = translate(&["IS_SEE_THROUGH"], &SEE_THROUGH_GLUE);
    let (device, _) = device().expect("a GPU");
    let text = CoreText::new(
        &device,
        wgpu::TextureFormat::Rgba8Unorm,
        &TextShaders {
            world: Some(Arc::new(world)),
            see_through: Some(Arc::new(see_through)),
            ..Default::default()
        },
    )
    .expect("the pack's world text pipelines");
    assert!(text.polygon_offset[0].is_some() && text.see_through[0].is_some());
}

#[test]
fn translated_world_text_binds_real_pages_in_normal_offset_and_see_through_runs() {
    let _gpu = crate::gfx::test_gpu::lock();
    let Some((device, queue)) = device() else {
        return;
    };
    let (_dir, atlas) = crate::font::marker_font_atlas(true);
    let mut core = CoreText::new(
        &device,
        wgpu::TextureFormat::Rgba8Unorm,
        &atlas.text_shaders,
    )
    .expect("pack world text programs");
    let mut batch = crate::hud::HudBatch::scratch([4, 2]);
    for (name, x) in [("one", 0.0), ("two", 2.0)] {
        batch.text_styled_world(
            &atlas,
            &crate::font::marker_text(name),
            x,
            -5.0,
            u32::MAX,
            false,
        );
    }
    let vertices: Vec<_> = batch
        .vertices
        .as_chunks::<6>()
        .0
        .iter()
        .flat_map(|quad| {
            [5, 2, 1, 0].map(|i| {
                let v = quad[i];
                v.world_text_vertex([v.pos[0] * 0.5 - 1.0, 1.0 - v.pos[1], 0.5], 0, 0)
            })
        })
        .collect();
    let pages = atlas.text_shaders.pages.as_ref().unwrap();
    pages.upload(&device, &queue);

    const SIDE: u32 = 64;
    let texture = |format, usage| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let light = texture(
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureUsages::TEXTURE_BINDING,
    )
    .create_view(&Default::default());
    core.prepare(
        &device,
        (&light, &device.create_sampler(&Default::default())),
    );
    let color = texture(
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let depth = texture(
        wgpu::TextureFormat::Depth32Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
    )
    .create_view(&Default::default());
    let color_view = color.create_view(&Default::default());
    let buffer = wgpu::util::DeviceExt::create_buffer_init(
        &device,
        &wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        },
    );
    let indices = wgpu::util::DeviceExt::create_buffer_init(
        &device,
        &wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&[0_u32, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        },
    );
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(SIDE * SIDE * 4),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    for mode in [Mode::Normal, Mode::PolygonOffset, Mode::SeeThrough] {
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations {
                        // A depth that only SEE_THROUGH can pass.
                        load: wgpu::LoadOp::Clear(if matches!(mode, Mode::SeeThrough) {
                            1.0
                        } else {
                            0.0
                        }),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            core.draw_runs(mode, &vertices, |range, binding| {
                let binding = binding.expect("each world run has its own font page");
                assert_eq!(range.end - range.start, 1);
                CoreText::draw_bound(&mut pass, &buffer, binding, range);
            });
        }
        encoder.copy_texture_to_buffer(
            color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &read,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIDE * 4),
                    rows_per_image: None,
                },
            },
            color.size(),
        );
        queue.submit([encoder.finish()]);
        read.slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        {
            let data = read.slice(..).get_mapped_range().unwrap();
            for (x, expected) in [
                (SIDE / 4, [0, 255, 0, 255]),
                (SIDE * 3 / 4, [0, 0, 255, 255]),
            ] {
                let at = ((SIDE / 2 * SIDE + x) * 4) as usize;
                assert_eq!(&data[at..at + 4], &expected);
            }
        }
        read.unmap();
    }
    eprintln!("normal, polygon-offset and see-through page pixels checked");
}

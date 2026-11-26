//! Original material-demand controls use finalized bindings, without a client or game launch.
use super::*;

#[test]
#[ignore = "requires a Vulkan/Metal/DX12 adapter"]
fn material_demand_follows_usable_bindings_and_custom_overrides() {
    let _gpu = crate::gfx::test_gpu::lock();
    let instance = crate::gfx::test_gpu::instance();
    let adapter = pollster::block_on(instance.request_adapter(&Default::default()))
        .expect("an offscreen adapter is required");
    eprintln!("Material demand adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_limits: adapter.limits(),
        ..Default::default()
    }))
    .unwrap();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Original demand control atlas"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let root = horizon_std::env::temp_dir().join(format!(
        "horizon-material-demand-bindings-{}",
        std::process::id()
    ));
    let vertex = horizon_shaders::GbuffersInputs {
        body: "hz_Vertex=vec4(0.0,0.0,0.0,1.0);".into(),
        ..Default::default()
    };
    let inputs = horizon_shaders::GameInputs {
        terrain: Some(vertex.clone()),
        sky: Some(vertex),
        ..Default::default()
    };
    let cases = [
        ("colour", "gbuffers_terrain", "", "vec4(0.4)", "", false),
        (
            "normal",
            "gbuffers_terrain",
            "uniform sampler2D normals;",
            "texture2D(normals,vec2(0.3))",
            "",
            true,
        ),
        (
            "specular",
            "gbuffers_terrain",
            "uniform sampler2D specular;",
            "texture2D(specular,vec2(0.3))",
            "",
            true,
        ),
        (
            "override",
            "gbuffers_terrain",
            "uniform sampler2D normals;",
            "texture2D(normals,vec2(0.3))",
            "texture.gbuffers.normals=minecraft:dynamic/lightmap_1",
            false,
        ),
        (
            "wrong_type",
            "gbuffers_terrain",
            "uniform sampler3D normals;",
            "texture3D(normals,vec3(0.3))",
            "",
            false,
        ),
        (
            "mixed",
            "gbuffers_terrain",
            "uniform sampler2D normals; uniform sampler2D specular;",
            "texture2D(normals,vec2(0.3))+texture2D(specular,vec2(0.3))",
            "texture.gbuffers.normals=minecraft:dynamic/lightmap_1",
            true,
        ),
        (
            "post_only",
            "final",
            "uniform sampler2D normals;",
            "texture2D(normals,vec2(0.3))",
            "",
            false,
        ),
        // A post pass reads the atlas's normal or specular map through a texture of the `_n`
        // or `_s` atlas resource: it needs the maps, under a name of the pack's own.
        (
            "post_atlas_normals",
            "composite",
            "uniform sampler2D atlasNormals;",
            "texture2D(atlasNormals,vec2(0.3))",
            "texture.composite.atlasNormals=minecraft:textures/atlas/blocks_n.png",
            true,
        ),
        (
            "post_atlas_specular",
            "composite",
            "uniform sampler2D atlasSpecular;",
            "texture2D(atlasSpecular,vec2(0.3))",
            "texture.composite.atlasSpecular=minecraft:textures/atlas/blocks_s.png",
            true,
        ),
        // The sky passes draw no terrain, but read the atlas's normal map through a texture.
        (
            "sky_atlas_normals",
            "gbuffers_skybasic",
            "uniform sampler2D atlasNormals;",
            "texture2D(atlasNormals,vec2(0.3))",
            "texture.gbuffers.atlasNormals=minecraft:textures/atlas/blocks_n.png",
            true,
        ),
        // Only the compute stage attached to `composite` reads the map; its fragment stage
        // reads none (the `.csh` is written below).
        (
            "compute_atlas_normals",
            "composite",
            "",
            "vec4(0.4)",
            "texture.composite.atlasNormals=minecraft:textures/atlas/blocks_n.png",
            true,
        ),
        (
            "unusable",
            "gbuffers_terrain",
            "uniform sampler2D normals;",
            "texture2D(normals,vec2(0.3))",
            "",
            false,
        ),
    ];
    for (case, program, declarations, sample, properties, expected) in cases {
        let path = root.join(case);
        horizon_std::fs::create_dir_all(path.join("shaders")).unwrap();
        horizon_std::fs::write(
            path.join(format!("shaders/{program}.vsh")),
            "#version 120\nvoid main(){gl_Position=gl_Vertex;}\n",
        )
        .unwrap();
        if case == "compute_atlas_normals" {
            horizon_std::fs::write(
                path.join("shaders/composite.csh"),
                "#version 430\nlayout(local_size_x=1) in;\nconst ivec3 workGroups=ivec3(1,1,1);\n\
                 uniform sampler2D atlasNormals;\nlayout(rgba8) writeonly uniform image2D colorimg5;\n\
                 void main(){imageStore(colorimg5,ivec2(0),texture(atlasNormals,vec2(0.3)));}\n",
            )
            .unwrap();
        }
        // Location 15 cannot be a colour attachment on this adapter. The GLSL still
        // translates, so this distinguishes authored demand from a usable renderer pass.
        let output = if case == "unusable" { 15 } else { 0 };
        if case == "unusable" {
            assert!(device.limits().max_color_attachments <= output);
        }
        horizon_std::fs::write(
            path.join(format!("shaders/{program}.fsh")),
            format!(
                "#version 120\n{declarations}\nvoid main(){{gl_FragData[{output}]={sample};}}\n"
            ),
        )
        .unwrap();
        let macros = horizon_shaders::Macros::standard();
        let properties = horizon_shaders::Properties::parse(properties, &macros);
        let pack = horizon_shaders::Pack::open(&path).unwrap();
        let post = horizon_shaders::load_post(&pack, "", &properties, &macros, &inputs);
        if program == "gbuffers_terrain" {
            assert!(post.terrain.is_some(), "{case}: {:?}", post.notes);
        }
        let built = PackPost::new(
            &device,
            &queue,
            &adapter,
            post,
            wgpu::TextureFormat::Rgba8Unorm,
            World {
                atlas: Some((view.clone(), [1, 1])),
                sprite_rects: &[],
                material_maps: None,
                lightmap: view.clone(),
                terrain: Some(Geometry {
                    buffers: &[],
                    table: None,
                    depth_write: true,
                    atlas: None,
                    material_maps: true,
                    ids: None,
                    cull: false,
                }),
                block_ids: &[],
                celestial: None,
                gui_size: [1, 1],
                clouds: None,
                weather: false,
                hand: false,
            },
            &Default::default(),
        );
        if case == "unusable" {
            match built {
                Ok(pack) => {
                    assert!(pack.terrain.is_none(), "{:?}", pack.notes);
                    assert!(!pack.needs_material_maps());
                }
                Err(refused) => eprintln!("Original unusable control refused: {refused:?}"),
            }
        } else {
            let pack = built.unwrap_or_else(|e| panic!("{case}: {e:?}"));
            assert_eq!(
                pack.needs_material_maps(),
                expected,
                "{case}: {:?}",
                pack.notes
            );
        }
    }
    horizon_std::fs::remove_dir_all(root).unwrap();
}

//! Original synthetic resource rows for the tag microbench and the ignored lava scene.
use super::world::{
    DATA_VERSION,
    fluid::{FluidKind, FluidState},
    level::{Behaviours, data},
    registry::{BlockRegistry, BlockStateId},
};
use horizon_core::{
    geometry::{Axis, VoxelShape},
    nbt::{Limits, NamedTag, Tag, write_named},
};
use serde_json::{Map, Value, json};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

// Behaviours' Named::resolve requires these identities even if a scene never
// sees them. Apart from the air/stone/liquid rows, they carry no gameplay data.
const NAMED_BLOCKS: &str = "
    air cave_air void_air water lava stone cobblestone obsidian basalt soul_soil blue_ice
    ice frosted_ice snow snow_block powder_snow dirt grass_block mycelium podzol farmland
    dirt_path mud clay sponge wet_sponge fire soul_fire redstone_wire comparator repeater
    redstone_torch redstone_wall_torch redstone_block redstone_lamp observer piston
    sticky_piston piston_head moving_piston tnt magma_block soul_sand bubble_column kelp
    kelp_plant seagrass tall_seagrass cactus cactus_flower sugar_cane bamboo bamboo_sapling
    scaffolding melon pumpkin carved_pumpkin jack_o_lantern barrier cake flower_pot cauldron
    water_cauldron lava_cauldron powder_snow_cauldron dragon_egg tripwire tripwire_hook
    slime_block honey_block hopper note_block chorus_plant chorus_flower end_stone vine
    lily_pad small_dripleaf big_dripleaf big_dripleaf_stem pointed_dripstone ladder
    sweet_berry_bush torchflower pitcher_plant pitcher_crop moss_block short_grass tall_grass
    fern large_fern dead_bush netherrack soul_campfire campfire light sculk_sensor
    calibrated_sculk_sensor sculk_shrieker sculk_catalyst sculk sculk_vein lectern target
    daylight_detector lever beehive bee_nest composter respawn_anchor end_portal_frame crafter
    copper_bulb glow_lichen hanging_roots mangrove_roots nether_portal chain_command_block
    frogspawn turtle_egg sniffer_egg sea_pickle coarse_dirt rooted_dirt crimson_nylium warped_nylium
";
const SYNTHETIC_BLOCKS: usize = 1_024;
const SYNTHETIC_TAGS: usize = 509;
pub(super) const TAG_COUNT: usize = SYNTHETIC_TAGS + 3;

struct Scratch(PathBuf);
impl Scratch {
    fn create() -> io::Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let path = std::env::temp_dir().join(format!(
            "horizon-tag-bench-{}-{}",
            std::process::id(),
            stamp.as_nanos(),
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_json(root: &Path, file: &str, value: Value) -> io::Result<()> {
    serde_json::to_writer(fs::File::create(root.join(file))?, &value).map_err(io::Error::other)
}

fn block_name(id: usize) -> String {
    format!("bench:block_{id:04}")
}

fn write_tags(root: &Path) -> io::Result<()> {
    let mut tags = Map::new();
    for (tag, members) in [
        ("fire", &(["minecraft:fire", "minecraft:soul_fire"])[..]),
        (
            "washed_away_by_fluids",
            &[
                "minecraft:air",
                "minecraft:cave_air",
                "minecraft:void_air",
                "minecraft:water",
                "minecraft:lava",
                "minecraft:fire",
                "minecraft:soul_fire",
            ][..],
        ),
        ("blocks_lava_fire_spread", &["minecraft:stone"][..]),
    ] {
        tags.insert(
            format!("minecraft:{tag}"),
            json!({ "values": members }).to_string().into(),
        );
    }
    // Vary tag key lengths and membership rather than loading an isolated key
    // or duplicating a captured game registry. All synthetic block names resolve.
    let families = [
        "ground",
        "connects_to",
        "surface_support",
        "valid_substrates",
    ];
    for i in 0..SYNTHETIC_TAGS {
        let members = (0..1 + i % 32)
            .map(|j| block_name((31 * i + 13 * j) % SYNTHETIC_BLOCKS))
            .collect::<Vec<_>>();
        tags.insert(
            format!("bench:{}/group_{i:03}", families[i % families.len()]),
            json!({ "values": members }).to_string().into(),
        );
    }
    assert_eq!(tags.len(), TAG_COUNT);
    write_json(root, "block-tags.json", Value::Object(tags))?;

    let fluid_tags = root.join("data/minecraft/tags/fluid");
    fs::create_dir_all(&fluid_tags)?;
    for (name, source, flowing) in [
        ("water", FluidKind::Water, FluidKind::FlowingWater),
        ("lava", FluidKind::Lava, FluidKind::FlowingLava),
    ] {
        write_json(
            &fluid_tags,
            &format!("{name}.json"),
            json!({ "values": [source.name(), flowing.name()] }),
        )?;
    }
    Ok(())
}

fn shape(value: VoxelShape, full: bool) -> Tag {
    let (min, max) = value.discrete_bounds();
    let mut tag = Tag::compound([
        ("canonical", Tag::Byte(if full { 2 } else { 1 })),
        ("cube", Tag::Byte(i8::from(value.is_cube()))),
        (
            "cube_axes",
            Tag::Byte(
                value
                    .cube_coordinate_axes()
                    .into_iter()
                    .enumerate()
                    .fold(0, |mask, (axis, cube)| mask | (i8::from(cube) << axis)),
            ),
        ),
        ("discrete_min", Tag::IntArray(min.into())),
        ("discrete_max", Tag::IntArray(max.into())),
        (
            "occupied",
            Tag::ByteArray(if full { vec![1] } else { vec![] }),
        ),
    ]);
    if let Tag::Compound(map) = &mut tag {
        for (axis, name) in Axis::ALL.into_iter().zip(["x", "y", "z"]) {
            map.insert(
                name.into(),
                Tag::LongArray(
                    value
                        .coordinates(axis)
                        .iter()
                        .map(|p| p.to_bits() as i64)
                        .collect(),
                ),
            );
        }
    }
    tag
}

pub(super) fn load() -> io::Result<(BlockRegistry, Behaviours)> {
    let scratch = Scratch::create()?;
    let root = &scratch.0;
    let mut blocks = Vec::new();
    let mut states = Vec::new();
    let mut defaults = Map::new();
    let mut gameplay = Vec::new();
    for name in NAMED_BLOCKS
        .split_whitespace()
        .map(|name| format!("minecraft:{name}"))
        .chain((0..SYNTHETIC_BLOCKS).map(block_name))
    {
        let liquid = matches!(name.as_str(), "minecraft:water" | "minecraft:lava");
        let air = matches!(
            name.as_str(),
            "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
        );
        let stone = name == "minecraft:stone";
        blocks
            .push(json!({ "name": name, "class": [if liquid { "LiquidBlock" } else { "Block" }] }));
        defaults.insert(name.clone(), json!(states.len()));
        for level in 0..if liquid { 16 } else { 1 } {
            let state = if liquid {
                json!({ "id": name, "properties": { "level": level.to_string() } })
            } else {
                json!(name)
            };
            states.push(json!([name, air, liquid, "", state]));
            gameplay.push(json!({
                "liquid": liquid,
                "replaceable": air || liquid,
                "can_occlude": stone,
                "full_collision": stone,
                "solid_render": stone,
            }));
        }
    }
    write_json(
        root,
        "facts.json",
        json!({ "states": states, "defaults": defaults }),
    )?;
    let registry = data::load_registry(root)?;
    let mut identities = Vec::with_capacity(registry.len());
    let mut cached_shapes = Vec::with_capacity(registry.len());
    let mut behaviour_rows = Vec::with_capacity(registry.len());
    for (id, game) in gameplay.iter().enumerate() {
        let state = registry.state(BlockStateId(id as u32)).unwrap();
        let stone = state.name == "minecraft:stone";
        identities.push(state.to_nbt());
        cached_shapes.push(i32::from(stone));
        let fluid = if game["liquid"] == true {
            FluidState::from_block(state)?
        } else {
            FluidState::EMPTY
        };
        let kind = FluidKind::ALL
            .iter()
            .position(|k| *k == fluid.kind())
            .unwrap();
        let flags = if stone {
            data::state_flags::SOLID
        } else if game["replaceable"] == true && fluid == FluidState::EMPTY {
            data::state_flags::AIR
        } else {
            0
        };
        behaviour_rows.push(json!([
            if stone { (1 << 18) - 1 } else { 0 },
            flags,
            kind,
            fluid.amount(),
            fluid.falling(),
        ]));
    }
    let collision = NamedTag {
        name: "".into(),
        tag: Tag::compound([
            ("DataVersion", Tag::Int(DATA_VERSION)),
            ("states", Tag::List(identities)),
            (
                "shapes",
                Tag::List(vec![
                    shape(VoxelShape::empty(), false),
                    shape(VoxelShape::block(), true),
                ]),
            ),
            ("cached_shapes", Tag::IntArray(cached_shapes)),
        ]),
    };
    fs::write(
        root.join("cached-collision.nbt"),
        write_named(&collision, Limits::tables()).map_err(io::Error::other)?,
    )?;

    let mut definitions = Map::new();
    for kind in FluidKind::ALL {
        let amount = if matches!(kind, FluidKind::FlowingWater | FluidKind::FlowingLava) {
            1
        } else {
            8
        };
        let default: Value = serde_json::from_str(&FluidState::new(kind, amount, false).to_json()?)
            .map_err(io::Error::other)?;
        let mut properties = Map::new();
        if let Some(values) = default["properties"].as_object() {
            properties.insert("falling".into(), json!(["false", "true"]));
            if values.contains_key("level") {
                properties.insert(
                    "level".into(),
                    json!((1..=8).map(|i| i.to_string()).collect::<Vec<_>>()),
                );
            }
        }
        definitions.insert(
            kind.name().into(),
            json!({ "properties": properties, "default": default }),
        );
    }
    write_json(
        root,
        "block-behaviours.json",
        json!({
            "states": behaviour_rows,
            "blocks": blocks,
            "items": {},
            "fluids": FluidKind::ALL.map(FluidKind::name),
            "fluid_state_definitions": definitions,
        }),
    )?;
    write_json(root, "gameplay-facts.json", json!({ "states": gameplay }))?;
    write_tags(root)?;
    let behaviours = Behaviours::load(&registry, root)?;
    Ok((registry, behaviours))
}

use super::*;
use crate::game::{api_values::TryIntoEngine, execution_role::ExecutionRole};
use horizon_client::lan::chunks::ChunkEncoder;
use horizon_core::nbt::{Compound, Tag};
use horizon_protocol::{
    buf::Reader,
    connection::Pipe,
    packets::play::{BlockEntityData, BlockUpdate, Clientbound, ForgetLevelChunk},
    registry::Builtin,
};
use horizon_std::sync::Mutex;
use mod_api::{
    BlockEntityDataReceivedEvent, BlockEntityPart, BlockEntityUpdateTagEvent, BlockPart,
};

const DATA: &str = "test:bottle";
const POISON: &str = r#"{id:"minecraft:splash_potion",count:1,components:{
    "minecraft:potion_contents":{potion:"minecraft:poison",custom_color:1973535,
        custom_effects:[{id:"minecraft:poison",duration:123,amplifier:1,show_particles:0b,show_icon:0b}],
        custom_name:"cauldron_probe"},"minecraft:custom_data":{proof:42}}}"#;
const TOP_UP: &str = r#"{id:"minecraft:lingering_potion",count:1,components:{
    "minecraft:potion_contents":{potion:"minecraft:poison",custom_color:1973536}}}"#;

#[derive(Default)]
struct CauldronPart(Arc<Mutex<Vec<Compound>>>);
#[mod_api::horizon_part]
impl BlockPart for CauldronPart {}
#[mod_api::horizon_part]
impl BlockEntityPart for CauldronPart {
    fn block_entity_update_tag(
        &self,
        event: &mut BlockEntityUpdateTagEvent<'_>,
    ) -> Option<Compound> {
        Some(event.entity.nbt.clone())
    }

    fn block_entity_data_received(
        &self,
        host: &dyn mod_api::ClientHost,
        event: &mut BlockEntityDataReceivedEvent<'_>,
    ) {
        self.0
            .lock()
            .unwrap()
            .push(event.entity.data(host).unwrap());
    }
}

struct Pour;
impl mod_api::Interaction for Pour {
    fn decide(&self, host: &dyn Host, input: &CauldronUse) -> CauldronDecision {
        let level = host.cauldron_level(input.state).unwrap_or(0) + 1;
        assert!(level <= 3);
        CauldronDecision::Success(CauldronPlan {
            block: host.block_state_id(&format!("minecraft:water_cauldron[level={level}]")),
            exchange: host.item_from_snbt(r#"{id:"minecraft:glass_bottle",count:1}"#),
            ..Default::default()
        })
    }

    fn fire(
        &self,
        host: &mut dyn ServerHost,
        input: &CauldronUse,
        _: &CauldronPlan,
    ) -> CauldronFire {
        let level = host.level(&input.dimension.to_string()).unwrap();
        let holder = host.block_entity(level, input.pos).unwrap();
        assert_eq!(
            holder.kind(host).unwrap().id(host),
            "test:cauldron_contents"
        );
        let stack = input.held.to_nbt(host).unwrap();
        assert!(holder.replace_data(host, bottle(stack)).unwrap());
        CauldronFire::Applied
    }
}

struct TestMod(CauldronPart);
#[mod_api::__subscriptions]
impl Hooks for TestMod {
    fn register(&self, r: &mut dyn mod_api::Registrar, _: &mod_api::RegistrationContext) {
        let holder = r
            .block_entity_type("cauldron_contents")
            .blocks(&["minecraft:water_cauldron"])
            .build();
        r.block_entity_part(&holder, CauldronPart(self.0.0.clone()))
            .unwrap();
        for block in ["minecraft:cauldron", "minecraft:water_cauldron"] {
            r.block_key(block);
        }
        for item in ["minecraft:splash_potion", "minecraft:lingering_potion"] {
            r.item_key(item);
        }
    }

    fn register_cauldron_interaction(
        &self,
        _: &mod_api::RegistrationContext,
        event: &mut mod_api::RegisterCauldronInteractionEvent<'_>,
    ) {
        for block in ["minecraft:cauldron", "minecraft:water_cauldron"] {
            for item in ["minecraft:splash_potion", "minecraft:lingering_potion"] {
                event
                    .interactions
                    .register(
                        mod_api::__private::block_key(block).unwrap(),
                        mod_api::__private::item_key(item).unwrap(),
                        Arc::new(Pour),
                    )
                    .unwrap();
            }
        }
    }
}

fn bottle(stack: Compound) -> Compound {
    [(DATA.into(), Tag::Compound(stack))].into()
}

fn stack(game: &Game<'_>, snbt: &str) -> (ItemStack, Compound) {
    let host: &dyn Host = game;
    let item = host.item_from_snbt(snbt).unwrap();
    (
        item.try_into_engine(game.items()).unwrap(),
        item.to_nbt(host).unwrap(),
    )
}

#[test]
#[ignore = "daily"]
fn native_water_cauldron_saves_syncs_and_rolls_back_the_complete_bottle() {
    if run_fresh_cases(
        "game::cauldron_interactions::tests::water_cauldron::native_water_cauldron_saves_syncs_and_rolls_back_the_complete_bottle",
        "HORIZON_OWNED_CAULDRON_CASE",
        &["run"],
    ) {
        return;
    }
    let seen = CauldronPart::default();
    let notices = seen.0.clone();
    let mods: mod_api::Mods = vec![mod_api::ModEntry::new("test", Box::new(TestMod(seen)))].into();
    mod_platform::__private::install(mods.clone());
    crate::game::api_values::install_parts(&mods).unwrap();
    let base =
        std::path::PathBuf::from(horizon_std::env::var_os("HORIZON_TEST_RESOURCES").unwrap());
    let overlay = crate::game::mods::content_root(&base, &Default::default()).unwrap();
    let root = overlay.prepared().to_path_buf();
    let builtin = Builtin::load(&Builtin::find(&root).unwrap()).unwrap();
    let kind = builtin
        .get("minecraft:block_entity_type")
        .unwrap()
        .id("test:cauldron_contents")
        .unwrap();
    let mut proof = None;
    with_game_at(root.clone(), |game| {
        setup(
            game,
            "minecraft:cauldron",
            None,
            "minecraft:splash_potion",
            1,
        );
        install(game.prepare_cauldron_interactions(game.items()).unwrap());
        let (poison_item, poison) = stack(game, POISON);
        let (Tag::Compound(expected), _) = horizon_core::nbt::parse_snbt(POISON).unwrap() else {
            unreachable!("fixture is a compound");
        };
        assert_eq!(
            poison.get(&"components".into()),
            expected.get(&"components".into())
        );
        assert_eq!(poison.get(&"id".into()), expected.get(&"id".into()));
        game.inventory[0] = Some(poison_item.clone());
        let level = game.level(game.dim.id()).unwrap();

        // Abort even the first fill: the tentative new holder cannot outlive the block.
        let aborted: std::result::Result<(), ()> = mod_api::run_transaction(game, |game, _| {
            assert!(game.use_on_cauldron(POS).unwrap());
            let holder = game.block_entity(level, POS).unwrap();
            assert_eq!(holder.data(game), Some(bottle(poison.clone())));
            Err(())
        });
        assert!(aborted.is_err());
        assert_eq!(state(game), ("minecraft:cauldron".into(), None));
        assert_eq!(game.inventory[0], Some(poison_item));
        assert!(game.block_entity(level, POS).is_none());
        assert!(game.use_on_cauldron(POS).unwrap());
        let original = game.block_entity(level, POS).unwrap();
        assert_eq!(
            state(game),
            ("minecraft:water_cauldron".into(), Some("1".into()))
        );
        assert_eq!(
            game.inventory[0].as_ref().unwrap().item.as_ref(),
            "minecraft:glass_bottle"
        );
        assert_eq!(original.data(game), Some(bottle(poison.clone())));

        let (top_up, new_bottle) = stack(game, TOP_UP);
        game.inventory[0] = Some(top_up.clone());
        game.world.block_log = Some(Vec::new());
        let abort: std::result::Result<(), ()> = mod_api::run_transaction(game, |game, _| {
            assert!(game.use_on_cauldron(POS).unwrap());
            assert_eq!(state(game).1.as_deref(), Some("2"));
            assert_eq!(original.data(game), Some(bottle(new_bottle.clone())));
            assert_eq!(
                game.inventory[0].as_ref().unwrap().item.as_ref(),
                "minecraft:glass_bottle"
            );
            Err(())
        });
        assert!(abort.is_err());
        assert_eq!(state(game).1.as_deref(), Some("1"));
        assert_eq!(game.inventory[0], Some(top_up));
        assert_eq!(original.data(game), Some(bottle(poison.clone())));
        assert!(game.world.block_log.as_ref().unwrap().is_empty());

        game.save_level(true).unwrap();
        game.world.block_entities.unload_chunks(&[(0, 0)]);
        game.world.chunks.remove(&(0, 0));
        assert_eq!(original.data(game), None);
        let mut chunk = game
            .save
            .load(0, 0, game.pack.assets.registry, world_biomes!(game))
            .unwrap();
        game.world.install_chunk_block_entities(&mut chunk);
        game.world.chunks.insert((0, 0), Arc::new(chunk));
        let reloaded = game.block_entity(level, POS).unwrap();
        assert_ne!(reloaded, original);
        assert_eq!(state(game).1.as_deref(), Some("1"));
        let read = reloaded.data(game).unwrap();
        let Some(Tag::Compound(item)) = read.get(&DATA.into()) else {
            panic!("the saved holder did not contain a bottle");
        };
        assert_eq!(
            mod_api::ItemStack::from_nbt(game, item)
                .unwrap()
                .to_nbt(game),
            Some(poison.clone())
        );

        let biomes: Vec<_> = world_biomes!(game)
            .list
            .iter()
            .map(|b| b.name.clone())
            .collect();
        let mut encoder = ChunkEncoder::new(
            game.res
                .facts()
                .states
                .iter()
                .map(|s| (s.name.clone(), s.fluid_level > 0 || s.waterlogged)),
            &builtin,
            &biomes,
            &biomes,
        );
        game.lan_update_tag_sources(&mut encoder);
        let bytes = encoder.body_in(&game.world.chunks[&(0, 0)], &game.world);
        let mut r = Reader::new(&bytes);
        let Clientbound::LevelChunkWithLight(packet) =
            Clientbound::decode(r.varint().unwrap(), r.rest()).unwrap()
        else {
            panic!("not a chunk");
        };
        assert_eq!(packet.chunk.block_entities.len(), 1);
        assert_eq!(packet.chunk.block_entities[0].kind, kind);
        assert_eq!(
            packet.chunk.block_entities[0].tag,
            Tag::Compound(bottle(poison.clone()))
        );

        assert!(game.use_on_cauldron(POS).unwrap());
        let state_two = game.world.block(0, 64, 0);
        let entity = game.world.block_entities.get(&(0, 64, 0)).unwrap();
        let tag = encoder.change_tag((0, 64, 0), state_two.0, entity).unwrap();
        assert_eq!(tag, Tag::Compound(bottle(new_bottle.clone())));
        let dirt = game
            .pack
            .assets
            .registry
            .default_state("minecraft:dirt")
            .unwrap();
        game.level_set(POS, dirt).unwrap();
        assert_eq!(reloaded.data(game), None);
        game.level_set(POS, state_two).unwrap();
        let replacement = game.block_entity(level, POS).unwrap();
        assert_eq!(replacement.data(game), Some(Compound::new()));
        proof = Some((packet, poison, new_bottle, state_two, dirt, tag));
    });

    let (packet, poison, new_bottle, state_two, dirt, tag) = proof.unwrap();
    crate::game::native_entity_refs::tests::with_game_role(root, ExecutionRole::Client, |game| {
        let (pipe, _server) = Pipe::pair();
        game.connect_local(pipe, "Cauldron").unwrap();
        let remote = game.remote.as_mut().unwrap();
        remote.logged_in = true;
        remote.builtin = Arc::new(builtin);
        let level = game.native_client_level().unwrap();
        let mut stock = packet.clone();
        stock.chunk.block_entities.clear();
        game.apply_packet(Clientbound::LevelChunkWithLight(stock))
            .unwrap();
        assert_eq!(state(game).1.as_deref(), Some("1"));
        assert!(game.block_entity(level, POS).is_none());
        game.apply_packet(Clientbound::LevelChunkWithLight(packet))
            .unwrap();
        let holder = game.block_entity(level, POS).unwrap();
        assert_eq!(holder.data(game), Some(bottle(poison.clone())));
        let send_block = |game: &mut Game<'_>, state: horizon_world::registry::BlockStateId| {
            game.apply_packet(Clientbound::BlockUpdate(BlockUpdate {
                pos: POS,
                state: state.0 as i32,
            }))
            .unwrap();
        };
        send_block(game, state_two);
        game.apply_packet(Clientbound::BlockEntityData(BlockEntityData {
            pos: POS,
            kind,
            tag,
        }))
        .unwrap();
        assert_eq!(holder.data(game), Some(bottle(new_bottle.clone())));
        assert_eq!(
            *notices.lock().unwrap(),
            [bottle(poison), bottle(new_bottle)]
        );
        send_block(game, dirt);
        assert_eq!(holder.data(game), None);
        send_block(game, state_two);
        assert!(game.block_entity(level, POS).is_none());
        game.apply_packet(Clientbound::ForgetLevelChunk(ForgetLevelChunk {
            pos: [0, 0],
        }))
        .unwrap();
        assert!(!game.world.loaded(0, 0));
    });
}

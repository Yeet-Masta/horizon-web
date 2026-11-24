//! `ChestBlock.useWithoutItem` awards `getOpenChestStat()` for every chest
//! block it serves; `CopperChestBlock` extends `ChestBlock` without overriding
//! it, so a copper chest counts as one opened, waxed or not
//! (`TrappedChestBlock.getOpenChestStat` is the only override). A barrel,
//! hopper, dropper, dispenser and shulker box award their own block's stat.
use super::Game;
use crate::game::native_machines::tests::server_game;
use horizon_client::containers::BlockEntity;
use horizon_client::mobs::{Species, Who};
use horizon_core::geometry::Vec3;
use horizon_core::nbt::Tag;

/// Puts `block` at `pos` with its block entity, as a player's click finds it.
fn place(game: &mut Game<'_>, pos: [i32; 3], block: &str) {
    let state = game
        .pack
        .assets
        .registry
        .default_state(block)
        .unwrap_or_else(|| panic!("{block} has no default state"));
    game.world.set(pos[0], pos[1], pos[2], state).unwrap();
    game.world.block_entities.insert(
        (pos[0], pos[1], pos[2]),
        BlockEntity::for_block(block).unwrap(),
    );
}

/// A `lock` naming an item the empty hand does not hold
/// (`BaseContainerBlockEntity.canOpen`).
fn lock(game: &mut Game<'_>, pos: [i32; 3]) {
    game.world.set_block_lock(
        (pos[0], pos[1], pos[2]),
        &Tag::compound([(
            "lock",
            Tag::compound([("items", Tag::String("minecraft:gold_ingot".into()))]),
        )]),
    );
}

/// Opens `block` at `pos` through the container path with an empty hand, and
/// leaves it open.
fn open(game: &mut Game<'_>, pos: [i32; 3], block: &str) {
    place(game, pos, block);
    game.open_block_effect_menu(pos);
    assert_eq!(
        game.container,
        Some((pos[0], pos[1], pos[2])),
        "{block} did not open"
    );
}

/// Opens a locked `block` with an empty hand, and checks it stayed shut.
fn open_locked(game: &mut Game<'_>, pos: [i32; 3], block: &str) {
    place(game, pos, block);
    lock(game, pos);
    game.titles.overlay = None;
    game.open_block_effect_menu(pos);
    assert_eq!(game.container, None, "{block} opened while locked");
    assert!(
        game.titles.overlay.is_some(),
        "{block}: no `container.isLocked` action bar"
    );
}

#[test]
#[ignore = "daily"]
fn a_copper_chest_counts_as_a_chest_opened() {
    let mut game = server_game();
    // `ChestBlock.useWithoutItem` -> `getOpenChestStat()`: `Stats.OPEN_CHEST`
    // for every chest block, `Stats.TRIGGER_TRAPPED_CHEST` for the trapped one.
    let chests = [
        "minecraft:chest",
        "minecraft:copper_chest",
        "minecraft:exposed_copper_chest",
        "minecraft:weathered_copper_chest",
        "minecraft:oxidized_copper_chest",
        "minecraft:waxed_copper_chest",
        "minecraft:waxed_exposed_copper_chest",
        "minecraft:waxed_weathered_copper_chest",
        "minecraft:waxed_oxidized_copper_chest",
    ];
    for (i, block) in chests.iter().enumerate() {
        let pos = [i as i32, 64, 8];
        open(&mut game, pos, block);
        assert_eq!(
            game.stats.get("custom", "minecraft:open_chest"),
            i as i32 + 1,
            "{block} did not count as a chest opened"
        );
    }
    // The trapped chest overrides the stat; the others have their own.
    open(&mut game, [0, 65, 8], "minecraft:trapped_chest");
    assert_eq!(
        game.stats.get("custom", "minecraft:trigger_trapped_chest"),
        1
    );
    for (i, (block, stat)) in [
        ("minecraft:barrel", "minecraft:open_barrel"),
        ("minecraft:shulker_box", "minecraft:open_shulker_box"),
        ("minecraft:hopper", "minecraft:inspect_hopper"),
        ("minecraft:dropper", "minecraft:inspect_dropper"),
        ("minecraft:dispenser", "minecraft:inspect_dispenser"),
    ]
    .iter()
    .enumerate()
    {
        let pos = [i as i32, 66, 8];
        open(&mut game, pos, block);
        assert_eq!(game.stats.get("custom", stat), 1, "{block}: no {stat}");
    }
    // No copper chest counted as a trapped chest.
    assert_eq!(
        game.stats.get("custom", "minecraft:trigger_trapped_chest"),
        1
    );
}

/// A refused `openMenu` does not stop the block's `useWithoutItem`: it awards
/// the open stat after `openMenu` returns (`ChestBlock`, `BarrelBlock`,
/// `ShulkerBoxBlock`, `HopperBlock`, `DispenserBlock`), and so do the
/// furnaces' `openContainer` and `BrewingStandBlock.useWithoutItem`.
#[test]
#[ignore = "daily"]
fn a_refused_locked_container_still_awards_its_open_stat() {
    let mut game = server_game();
    for (i, (block, stat)) in [
        ("minecraft:chest", "minecraft:open_chest"),
        ("minecraft:copper_chest", "minecraft:open_chest"),
        ("minecraft:trapped_chest", "minecraft:trigger_trapped_chest"),
        ("minecraft:barrel", "minecraft:open_barrel"),
        ("minecraft:shulker_box", "minecraft:open_shulker_box"),
        ("minecraft:hopper", "minecraft:inspect_hopper"),
        ("minecraft:dropper", "minecraft:inspect_dropper"),
        ("minecraft:dispenser", "minecraft:inspect_dispenser"),
        ("minecraft:furnace", "minecraft:interact_with_furnace"),
        (
            "minecraft:blast_furnace",
            "minecraft:interact_with_blast_furnace",
        ),
        ("minecraft:smoker", "minecraft:interact_with_smoker"),
        ("minecraft:brewing_stand", "minecraft:interact_with_brewingstand"),
    ]
    .iter()
    .enumerate()
    {
        let pos = [i as i32, 64, 8];
        let before = game.stats.get("custom", stat);
        open_locked(&mut game, pos, block);
        assert_eq!(
            game.stats.get("custom", stat),
            before + 1,
            "{block}: a refused open awarded no {stat}"
        );
    }
}

/// `ShulkerBoxBlock.useWithoutItem` asks `canOpen` (room for the lid) before
/// `openMenu`, so a locked box under a block shows no lock message.
#[test]
#[ignore = "daily"]
fn a_locked_shulker_box_with_no_room_for_its_lid_says_nothing() {
    let mut game = server_game();
    let pos = [8, 64, 8];
    let stone = game
        .pack
        .assets
        .registry
        .default_state("minecraft:stone")
        .unwrap();
    game.world.set(8, 65, 8, stone).unwrap();
    place(&mut game, pos, "minecraft:shulker_box");
    lock(&mut game, pos);
    game.titles.overlay = None;
    game.open_block_effect_menu(pos);
    assert_eq!(game.container, None, "the box opened");
    assert!(
        game.titles.overlay.is_none(),
        "a box that cannot open said it is locked"
    );
    assert_eq!(
        game.stats.get("custom", "minecraft:open_shulker_box"),
        0,
        "a box that cannot open counted as opened"
    );
}

/// `ChestBlock.useWithoutItem` angers the piglins that see the player even
/// when `createMenu` refused the lock; `HopperBlock.useWithoutItem` has no
/// `PiglinAi.angerNearbyPiglins`, so a refused hopper leaves them calm.
#[test]
#[ignore = "daily"]
fn a_refused_locked_chest_angers_the_piglins_that_see_it() {
    let mut resources = None;
    let mut game = crate::game::game_tests::tests::server_game(&mut resources);
    game.build_registries();
    game.set_allow_commands(true);
    game.player = Some(
        horizon_client::motion::PlayerMotion::new(game.motion, Vec3::new(8.5, 64.0, 8.5))
            .expect("a player position"),
    );
    assert!(game.run_command("summon piglin 8.5 64 12.5 {NoAI:1b,PersistenceRequired:1b}"));
    let piglin = game
        .mobs
        .iter()
        .position(|m| m.species() == Species::Piglin)
        .expect("the summoned piglin");

    open_locked(&mut game, [8, 64, 8], "minecraft:chest");
    let m = &game.mobs[piglin];
    assert_eq!(
        m.ai.target,
        Some(Who::Player),
        "a refused chest open left the piglin calm"
    );
    assert_eq!(m.extra.anger, 600, "PiglinAi.setAngerTarget's 600 ticks");

    // The hopper's `useWithoutItem` never calls `angerNearbyPiglins`.
    let m = &mut game.mobs[piglin];
    m.extra.anger = 0;
    m.ai.target = None;
    m.extra.piglin.guarded_angry_at = None;
    open_locked(&mut game, [9, 64, 8], "minecraft:hopper");
    assert!(
        game.mobs[piglin].extra.piglin.guarded_angry_at.is_none(),
        "a refused hopper angered the piglin"
    );
}

//! `EndGatewayBlock.entityInside` has no dimension test: a gateway outside
//! the End still takes an entity in and starts its cooldown, but
//! `TheEndGatewayBlockEntity.getPortalPosition` only builds an exit in the
//! End. A move is swept, so a fast fall cannot pass through between ticks.

use crate::game::Dim;
use crate::game::end::GatewayJob;
use crate::game::native_machines::tests::server_game;
use horizon_client::chunk::Chunk;
use horizon_client::containers::{BlockEntity, EndGateway};
use horizon_client::entities::ItemEntity;
use horizon_client::items::ItemStack;
use horizon_core::geometry::Vec3;
use std::sync::Arc;

#[test]
#[ignore = "daily"]
fn an_item_in_a_gateway_outside_the_end_still_enters_it() {
    let mut game = server_game();
    game.dim = Dim::Overworld;
    game.settings.simulation_distance = 4;
    game.settings.render_distance = 12;
    let gateway = game
        .pack
        .assets
        .registry
        .default_state("minecraft:end_gateway")
        .unwrap();
    game.player = Some(
        horizon_client::motion::PlayerMotion::new(game.motion, Vec3::new(8.5, 65.0, 8.5)).unwrap(),
    );
    game.load_ticking_neighbourhood(0, 0);
    game.world.set(8, 64, 8, gateway).unwrap();
    game.world
        .block_entities
        .insert((8, 64, 8), BlockEntity::EndGateway(Default::default()));
    game.dropped.push(ItemEntity::new(
        1,
        ItemStack::new("minecraft:stone", 1),
        Vec3::new(8.5, 64.2, 8.5),
        Vec3::ZERO,
        0.0,
    ));
    game.update_chunk_tickets();
    assert!(game.entity_ticking_area()(8, 8));

    game.find_end_gateway_portals();

    assert!(
        !game.gateway_available([8, 64, 8]),
        "the item went into the gateway outside the End and started its cooldown"
    );
    // Entity.handlePortal next tick sets the item's cooldown without an exit.
    game.handle_end_gateway_portals();
    assert!(
        game.end.arrivals.is_empty(),
        "no destination outside the End"
    );
    assert!(
        game.end.gateways.get(&1).is_some_and(|t| t.cooldown() > 0),
        "the item's portal cooldown is set even without a destination"
    );
    assert_eq!(
        game.gateway_job([8, 64, 8]).0,
        GatewayJob::NoDestination,
        "getPortalPosition creates no exit outside the End"
    );
}

#[test]
#[ignore = "daily"]
fn a_data_less_gateway_in_the_outer_end_searches_outward() {
    let mut game = server_game();
    game.dim = Dim::End;
    let (job, target) = game.gateway_job([702, 100, 0]);
    assert_eq!(
        job,
        GatewayJob::Find {
            gateway: [702, 100, 0],
            exact: false
        },
        "a gateway without an exit searches, however far out it is"
    );
    assert!((target.x - 1024.0).abs() < 1e-9, "{target:?}");
    assert_eq!((target.y, target.z), (75.0, 0.0));
}

/// The return gateway worldgen places on the outer islands carries its exit:
/// `end_gateway_return` is `EndGatewayFeature.knownExit(ServerLevel.END_SPAWN_POINT, true)`,
/// so `EndGatewayFeature.place` gives its block entity an `exit_portal` and
/// `ExactTeleport` (`setExitPosition`), and it still takes a traveller back to
/// the spawn island now that the far-out `END_SPAWN` shortcut is gone.
#[test]
#[ignore = "daily"]
fn a_worldgen_return_gateway_on_an_outer_island_leads_to_the_spawn_island() {
    let mut game = server_game();
    game.dim = Dim::End;
    let data = horizon_world::worldgen::generator::GeneratorData::load(
        &game.options.resources.join("worldgen"),
        game.pack.assets.registry,
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(
        data.configured_feature_json("minecraft:end_gateway_return")
            .expect("the placed return gateway is in the pack"),
    )
    .unwrap();
    assert_eq!(json["exact"], serde_json::Value::Bool(true));
    let exit: [i32; 3] = serde_json::from_value(json["exit"].clone()).unwrap();
    assert_eq!(exit, [100, 50, 0], "ServerLevel.END_SPAWN_POINT");
    // EndGatewayFeature.place: setExitPosition(exit, exact) on the new block
    // entity. The gateway sits on an outer island, far past the old 512 blocks.
    let gateway = [702, 80, 0];
    game.world.block_entities.insert(
        (gateway[0], gateway[1], gateway[2]),
        BlockEntity::EndGateway(EndGateway {
            exit_portal: Some(exit),
            exact_teleport: true,
            ..Default::default()
        }),
    );
    let (job, target) = game.gateway_job(gateway);
    assert_eq!(job, GatewayJob::Known { exit, exact: true });
    assert_eq!(
        game.resolve_gateway(job, target).ok(),
        Some(Some(Vec3::new(100.5, 50.0, 0.5))),
        "the return gateway lands at the spawn island's exit portal"
    );
}

/// `findExitPosition`'s `findTallestBlock` reads the destination level's own
/// build height (`Level.getMaxY()` down to `Level.getMinY()`): a configured
/// non-exact exit outside the End lands above a block above y = 255, and above
/// one below y = 1, instead of on the fallback a fixed Y = 255..1 scan returns.
#[test]
#[ignore = "daily"]
fn a_non_exact_exit_outside_the_end_searches_the_levels_own_height() {
    let mut game = server_game();
    game.dim = Dim::Overworld;
    // findExitPosition reads 5 blocks around the exit: chunks (12, -1) and (12, 0).
    for c in [(12, -1), (12, 0)] {
        game.world
            .chunks
            .insert(c, Arc::new(Chunk::empty(c.0, c.1)));
    }
    let stone = game
        .pack
        .assets
        .registry
        .default_state("minecraft:stone")
        .unwrap();
    let air = game
        .pack
        .assets
        .registry
        .default_state("minecraft:air")
        .unwrap();
    let exit = [200, 75, 0];
    let gateway = game
        .pack
        .assets
        .registry
        .default_state("minecraft:end_gateway")
        .unwrap();
    game.world.set(0, 64, 0, gateway).unwrap();
    game.world.block_entities.insert(
        (0, 64, 0),
        BlockEntity::EndGateway(EndGateway {
            exit_portal: Some(exit),
            exact_teleport: false,
            ..Default::default()
        }),
    );
    let (job, target) = game.gateway_job([0, 64, 0]);
    assert_eq!(job, GatewayJob::Known { exit, exact: false });

    // The Overworld's build height is -64 to 319.
    game.world.set(199, 300, -2, stone).unwrap();
    assert_eq!(
        game.resolve_gateway(job, target).ok(),
        Some(Some(Vec3::new(199.5, 301.0, -1.5))),
        "the landing is above the tallest block, above y = 255"
    );
    game.world.set(199, 300, -2, air).unwrap();

    game.world.set(201, -40, 2, stone).unwrap();
    assert_eq!(
        game.resolve_gateway(job, target).ok(),
        Some(Some(Vec3::new(201.5, -39.0, 2.5))),
        "the landing is above the tallest block, below y = 1"
    );
}

#[test]
#[ignore = "daily"]
fn a_fast_fall_through_a_gateway_outside_the_end_is_caught() {
    let mut resources = None;
    let (mut game, scratch) =
        super::server_travel::credits_respawn_tests::server_with_a_nether_anchor(&mut resources);
    let gateway = game
        .pack
        .assets
        .registry
        .default_state("minecraft:end_gateway")
        .unwrap();
    game.world.set(2, 64, 2, gateway).unwrap();
    game.world
        .block_entities
        .insert((2, 64, 2), BlockEntity::EndGateway(Default::default()));
    // A fall of 3.9 blocks in one tick, from above the gateway to below it.
    game.lan.as_mut().unwrap().hub.guests.update(0, |g| {
        g.previous_position = [2.5, 66.0, 2.5];
        g.first_good = [2.5, 66.0, 2.5];
        g.position = [2.5, 62.1, 2.5];
    });

    game.handle_server_portal(2);

    assert!(
        !game.gateway_available([2, 64, 2]),
        "the fall swept through the gateway"
    );
    assert_eq!(
        game.portals.player.cooldown, 10,
        "the gateway started the player's portal cooldown"
    );
    assert!(
        game.server_trips.is_empty(),
        "a gateway without an exit outside the End has no destination"
    );
    // Standing still below it, the next ticks sweep no move: the fall is not
    // swept again when the gateway's own cooldown runs out.
    for _ in 0..40 {
        game.tick_end_gateway((2, 64, 2));
        game.lan
            .as_mut()
            .unwrap()
            .hub
            .guests
            .update(0, |g| g.first_good = g.position);
        game.handle_server_portal(2);
    }
    assert_eq!(
        game.portals.player.cooldown, 0,
        "the fall through the gateway is swept once, not every tick"
    );
    drop(game);
    horizon_std::fs::remove_dir_all(scratch).unwrap();
}

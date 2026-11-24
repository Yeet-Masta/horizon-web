//! A dragon-fight gateway whose 3x3 footprint crosses a chunk that isn't
//! loaded waits, saved with the fight, and is placed whole in the End once that
//! chunk loads (`EndGatewayFeature.place` writes every block of the cross).

use crate::game::Dim;
use crate::game::game_tests::tests::server_game;
use horizon_client::chunk::Chunk;
use std::sync::Arc;

#[test]
#[ignore = "daily"]
fn a_queued_gateway_survives_a_save_and_is_placed_whole_only_in_the_end() {
    let mut resources = None;
    let mut game = server_game(&mut resources);
    let world = std::env::temp_dir().join(format!("horizon-pending-gateway-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&world);
    game.world_root = world.clone();
    game.dim = Dim::End;
    game.fight_ready();
    // The gateway at x 15 reaches x 16, in chunk (1, 0), which isn't loaded.
    let origin = [15, 75, 8];
    game.world
        .chunks
        .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    game.place_gateway_at(origin).unwrap();
    assert_ne!(&*game.block_name(origin), "minecraft:end_gateway");
    assert_eq!(game.end.pending_gateways, vec![origin]);

    // The queue is in ender_dragon_fight.dat: a reopened world still has it.
    game.save_fight();
    game.end.pending_gateways.clear();
    game.fight = Default::default();
    game.fight_ready();
    assert_eq!(game.end.pending_gateways, vec![origin]);

    game.world
        .chunks
        .insert((1, 0), Arc::new(Chunk::empty(1, 0)));
    // Another level leaves the End's queue alone.
    game.dim = Dim::Overworld;
    game.tick_pending_gateways().unwrap();
    assert_eq!(game.end.pending_gateways, vec![origin]);
    assert_ne!(&*game.block_name(origin), "minecraft:end_gateway");

    game.dim = Dim::End;
    game.tick_pending_gateways().unwrap();
    assert!(game.end.pending_gateways.is_empty());
    assert_eq!(&*game.block_name(origin), "minecraft:end_gateway");
    // The bedrock on the far side of the chunk border.
    assert_eq!(&*game.block_name([16, 74, 8]), "minecraft:bedrock");
    assert_eq!(&*game.block_name([16, 76, 8]), "minecraft:bedrock");
    let _ = std::fs::remove_dir_all(&world);
}

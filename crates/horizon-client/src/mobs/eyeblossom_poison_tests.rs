//! `EyeblossomBlock.entityInside` through `Entity.applyEffectsFromBlocks`: a
//! bee standing in an open eyeblossom takes poison for 25 ticks, unless the
//! level's difficulty is Peaceful.
use crate::mobs::Mob;
use crate::mobs::ai::{flower_contact_effects, register};
use horizon_core::geometry::Vec3;
use horizon_world::registry::BlockRegistry;
use std::sync::Arc;

/// An empty world with `block` at (0, 64, 0), where the bee stands.
fn build_with(block: &'static str) -> impl FnOnce(&mut crate::world::World, &BlockRegistry) {
    move |world, registry| {
        let state = registry.default_state(block).unwrap();
        for cx in -1..1 {
            for cz in -1..1 {
                world
                    .chunks
                    .insert((cx, cz), Arc::new(crate::chunk::Chunk::empty(cx, cz)));
            }
        }
        world.set(0, 64, 0, state).unwrap();
    }
}

/// A bee whose box is inside the flower's cell.
fn bee(types: &crate::mobs::MobTypes) -> Mob {
    let kind = Arc::new(types.types["minecraft:bee"].clone());
    let mut m = Mob::new(1, kind, Vec3::new(0.5, 64.0, 0.5), 0.0);
    register(&mut m);
    m
}

/// `EyeblossomBlock.entityInside` asks `level.getDifficulty() !=
/// Difficulty.PEACEFUL` before it poisons a bee: the same bee, at the same
/// place, only takes the effect above Peaceful.
#[test]
#[ignore = "daily"]
fn a_bee_in_an_open_eyeblossom_is_poisoned_except_on_peaceful() {
    let ran = crate::mobs::world::with_built_test_world(
        &[],
        build_with("minecraft:open_eyeblossom"),
        |w, types| {
            assert_eq!(w.difficulty.base, 2, "normal difficulty");
            let mut m = bee(types);
            flower_contact_effects(&mut m, w);
            assert!(m.has_effect("minecraft:poison"), "poisoned on normal");

            let mut peaceful = w.clone();
            peaceful.difficulty = crate::mobs::spawning::LocalDifficulty::new(0, 18000, 0, 0.0);
            let mut m = bee(types);
            flower_contact_effects(&mut m, &peaceful);
            assert!(!m.has_effect("minecraft:poison"), "untouched on peaceful");
        },
    );
    assert!(ran, "vanilla data is missing");
}

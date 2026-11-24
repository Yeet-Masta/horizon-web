//! `CombatTracker.getDeathMessage` details: the damage type's own message key
//! (a trident is not an arrow), `getFallMessage`'s comparison of display names,
//! and the intentional-game-design link's click and hover.
use super::*;
use crate::game::native_machines::tests::server_game;
use horizon_client::mobs::{Mob, Who};
use horizon_client::motion::PlayerMotion;
use horizon_client::projectiles::{Hit, Projectile, ProjectileKind};
use horizon_core::geometry::Vec3;

/// A tamed wolf of the local player, killed by a drowned's trident: the owner
/// is told `death.attack.trident` ("was impaled by"), not the arrow's message.
#[test]
#[ignore = "daily"]
fn a_trident_kills_with_the_trident_message() {
    let mut game = server_game();
    game.options.dedicated = None;
    game.build_registries();
    game.player = Some(PlayerMotion::new(game.motion, Vec3::new(0.5, 64.0, 0.5)).unwrap());
    let owner = [0, 0, 0, 1];
    game.level.player_uuid = Some(owner);
    let drowned = game
        .res
        .mob_types()
        .get("minecraft:drowned")
        .unwrap()
        .clone();
    game.mobs
        .push(Mob::new(90, drowned, Vec3::new(5.5, 64.0, 0.5), 0.0));
    let wolf = game.res.mob_types().get("minecraft:wolf").unwrap().clone();
    let mut pet = Mob::new(91, wolf, Vec3::new(0.5, 64.0, 0.5), 0.0);
    pet.health = 1.0;
    pet.extra.tame = true;
    pet.extra.owner = Some(owner);
    game.mobs.push(pet);
    game.chat_capture = Some(Vec::new());
    let trident = Projectile::new(
        ProjectileKind::Trident,
        Vec3::new(0.5, 64.0, 0.5),
        Some(Who::Mob(90)),
    );
    game.projectile_hit(
        &trident,
        Hit::Entity {
            who: Who::Mob(91),
            at: trident.position,
        },
    )
    .unwrap();
    let pet = game.mobs.iter().find(|m| m.id == 91).unwrap().clone();
    assert!(!pet.alive(), "the trident's 8 damage must kill a wolf at 1");
    // A real projectile is the direct entity, named by the damage type.
    assert_eq!(pet.last_direct, None);
    let view = game.damage_view(pet.last_damage.unwrap().0);
    assert_eq!(view.direct.as_ref().unwrap().kind, "minecraft:trident");
    assert!(!view.is_direct, "the trident is not its owner");
    // TamableAnimal.die tells the owner how it died.
    game.tell_owner_pet_died(&pet, None);
    let said = game.chat_capture.take().unwrap_or_default();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(
        said[0].translation_key(),
        Some("death.attack.trident"),
        "{}",
        said[0].plain()
    );
}

/// `CombatTracker.getFallMessage` compares display names: two unnamed zombies
/// are the same attacker, so the last blow finishes the fall. The killing blow
/// is a `fall` an entity caused, as `/damage <target> minecraft:fall from
/// <zombie>` builds one.
#[test]
#[ignore = "daily"]
fn two_unnamed_zombies_are_the_same_attacker() {
    let mut game = server_game();
    game.build_registries();
    game.player = Some(PlayerMotion::new(game.motion, Vec3::new(0.5, 64.0, 0.5)).unwrap());
    let knock_off = game
        .health_resources
        .damage
        .id("minecraft:mob_attack")
        .unwrap();
    let fall = game.health_resources.damage.id("minecraft:fall").unwrap();
    let zombie = game
        .res
        .mob_types()
        .get("minecraft:zombie")
        .unwrap()
        .clone();
    for id in [41, 42] {
        game.mobs
            .push(Mob::new(id, zombie.clone(), Vec3::new(3.5, 64.0, 0.5), 0.0));
    }
    let hit = |source, amount| {
        vec![MotionEvent::Health(HealthEvent::RecordedDamage {
            source,
            amount,
        })]
    };
    // Zombie 41 knocks the player off; the fall is 8 blocks; zombie 42, of the
    // same type and unnamed, lands a fall hit on the way down.
    game.note_attacker("minecraft:mob_attack", 41);
    game.record_combat(&hit(knock_off, 1.0));
    game.player.as_mut().unwrap().fall_distance = 8.0;
    game.record_combat(&hit(fall, 8.0));
    game.note_attacker("minecraft:fall", 42);
    game.record_combat(&hit(fall, 1.0));
    let text = game.combat_death_message_for(Text::literal("Victim"));
    assert_eq!(
        text.translation_key(),
        Some("death.fell.finish"),
        "{}",
        text.plain()
    );
}

/// `bad_respawn_point` is an intentional game design: the bracketed link opens
/// the bug tracker and shows `MCPE-28723`, on the death screen as in chat.
#[test]
#[ignore = "daily"]
fn the_bad_respawn_point_link_carries_its_click_and_hover() {
    let mut game = server_game();
    game.build_registries();
    let source = game
        .health_resources
        .damage
        .id("minecraft:bad_respawn_point")
        .unwrap();
    game.record_combat(&[MotionEvent::Health(HealthEvent::RecordedDamage {
        source,
        amount: 5.0,
    })]);
    let text = game.combat_death_message_for(Text::literal("Victim"));
    assert_eq!(
        text.translation_key(),
        Some("death.attack.badRespawnPoint.message"),
        "{}",
        text.plain()
    );
    let component = text.component();
    let link = &component["with"][1];
    assert_eq!(link["click_event"]["action"], "open_url");
    assert_eq!(
        link["click_event"]["url"],
        "https://bugs.mojang.com/browse/MCPE-28723"
    );
    assert_eq!(link["hover_event"]["action"], "show_text");
    assert_eq!(link["hover_event"]["value"]["text"], "MCPE-28723");
    // What the death screen's `ClickableStyleFinder` reads: the bracketed
    // text's own spans.
    let clickable: String = text
        .spans
        .iter()
        .filter(|s| s.style.click.is_some())
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(clickable, "[Intentional Game Design]");
}

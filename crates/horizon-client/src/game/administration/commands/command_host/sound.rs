use super::super::Target;
use super::entity::{entity_ref, target_of};
use crate::game::Game;
use horizon_core::random::RandomSource;
use horizon_gameplay::commands::{entity::EntityRef, sound::SoundHost};

impl SoundHost for Game<'_> {
    fn source_player(&self) -> Option<EntityRef> {
        self.source_entity()
            .filter(|t| t.is_player())
            .map(entity_ref)
    }
    fn hears_sounds(&self, entity: EntityRef) -> bool {
        let t = target_of(entity);
        if !matches!(t, Target::Player | Target::Guest(_)) {
            return false;
        }
        // Only players in the source's level hear it.
        let here = self.dim.id();
        if let Target::Guest(conn) = t
            && self.command_guest(conn).is_none_or(|g| g.dimension != here)
        {
            return false;
        }
        true
    }
    fn sound_seed(&mut self) -> i64 {
        self.world.random.next_long()
    }
    fn play_sound_to(
        &mut self,
        entity: EntityRef,
        sound: &str,
        at: [f64; 3],
        volume: f32,
        pitch: f32,
        source: usize,
        seed: i64,
    ) {
        Game::play_sound_to(
            self,
            target_of(entity),
            sound,
            at,
            volume,
            pitch,
            source,
            seed,
        );
    }
    fn stop_sound_for(
        &mut self,
        targets: &[EntityRef],
        sound: Option<&str>,
        source: Option<usize>,
    ) {
        let targets: Vec<Target> = targets.iter().map(|&e| target_of(e)).collect();
        Game::stop_sound_for(self, &targets, sound, source);
    }
}

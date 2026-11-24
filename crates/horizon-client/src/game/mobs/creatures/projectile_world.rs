//! What a flying projectile asks of the world (`ProjectileWorld` for `ProjWorld`): block
//! contacts, collisions, fluids and the world border.
use super::*;

pub(crate) struct ProjWorld<'a, 'w> {
    pub(crate) w: &'a MobWorld<'w>,
    pub(crate) targets: &'a [(Who, Aabb)],
    /// The world border's [minX, minZ, maxX, maxZ], which stops the ray as a
    /// block would (`clipIncludingBorder`); None: no border.
    pub(crate) border: Option<[f64; 4]>,
}

impl ProjectileWorld for ProjWorld<'_, '_> {
    fn below_world_y(&self) -> f64 {
        self.w.world.below_world_y()
    }
    fn clip(&self, from: Vec3, to: Vec3) -> Option<(Vec3, [i32; 3], [i32; 3])> {
        let mut scene = SceneCollisions {
            world: self.w.world,
            binding: self.w.collision,
            subject: None,
        };
        let context = horizon_world::collision_context::CollisionContext::position(from.y);
        let r = traverse_blocks(from, to, 256, |p: BlockPos| {
            let state = self.w.world.block(p.x, p.y, p.z);
            if self.w.facts.is_air(state) {
                return None;
            }
            // ClipContext hands CollisionContext.of(projectile): scaffolding
            // is open for a projectile below it.
            let shape = self
                .w
                .collision
                .collision_shape_with_context(&mut scene, state, p, &context)
                .ok()?;
            shape.clip(from, to, p)
        });
        let hit = r.ok().flatten().map(|hit| {
            (
                hit.location,
                [hit.block.x, hit.block.y, hit.block.z],
                hit.direction.normal(),
            )
        });
        match self.border {
            Some(bounds) => clip_including_border(hit, from, to, bounds),
            None => hit,
        }
    }
    /// Entity.updateFluidInteraction over the projectile's box, not the
    /// block at its feet (#3072).
    fn in_water(&self, bounds: Aabb) -> bool {
        self.w.fluids(bounds, bounds.max_position().y).in_water
    }
    fn pushed_by_currents(&self, bounds: Aabb, velocity: Vec3) -> Vec3 {
        self.w.pushed_by_currents(bounds, velocity)
    }
    fn targets(&self) -> &[(Who, Aabb)] {
        self.targets
    }
    /// `BaseFireBlock.entityInside` for a thrown projectile: fire and soul
    /// fire set it alight for 160 ticks.
    fn ignites(&self, p: [i32; 3]) -> bool {
        let state = self.w.world.block(p[0], p[1], p[2]);
        matches!(
            &*self.w.facts.get(state).name,
            "minecraft:fire" | "minecraft:soul_fire"
        )
    }
    fn empty(&self, p: [i32; 3]) -> bool {
        self.w.facts.is_air(self.w.world.block(p[0], p[1], p[2]))
    }
    fn inside_collision(&self, at: Vec3) -> bool {
        let p = BlockPos::containing(at);
        let state = self.w.world.block(p.x, p.y, p.z);
        if self.w.facts.is_air(state) {
            return false;
        }
        let mut scene = SceneCollisions {
            world: self.w.world,
            binding: self.w.collision,
            subject: None,
        };
        let origin = Vec3::new(f64::from(p.x), f64::from(p.y), f64::from(p.z));
        self.w
            .collision
            .collision_shape(&mut scene, state, [p.x, p.y, p.z])
            .is_ok_and(|s| s.boxes().iter().any(|b| b.move_by(origin).contains(at)))
    }
    /// `WebBlock.entityInside` and `PowderSnowBlock.entityInside`.
    fn stuck_speed(&self, p: [i32; 3]) -> Option<Vec3> {
        horizon_client::projectiles::stuck_speed_of(
            &self.w.facts.get(self.w.world.block(p[0], p[1], p[2])).name,
        )
    }
    fn bubble_column(&self, p: [i32; 3]) -> Option<(bool, bool)> {
        horizon_client::motion::bubble_column_at(
            self.w.world,
            self.w.registry,
            self.w.facts,
            self.w.collision,
            BlockPos::new(p[0], p[1], p[2]),
        )
        .map(|c| (c.drag, c.surface))
    }
    fn can_stand_on(&self, p: [i32; 3], entity_y: f64) -> bool {
        horizon_client::collisions::projectile_can_stand_on(
            self.w.world,
            self.w.collision,
            BlockPos::new(p[0], p[1], p[2]),
            entity_y,
        )
    }
}

impl Game<'_> {
    /// `Entity.getTeam` of a player a projectile can name: the host under
    /// `Who::Player`, a guest under its entity id. `None` for a mob, which
    /// `AbstractArrow.canHitEntity` does not team-check.
    fn player_team_of(&self, who: Who) -> Option<Option<&horizon_client::scoreboard::Team>> {
        match who {
            Who::Player => Some(self.player_team()),
            Who::Mob(id) => self
                .lan
                .as_ref()
                .and_then(|h| h.hub.guests.iter().find(|g| g.entity_id as u32 == id))
                .map(|g| self.score.board.players_team(&g.name)),
        }
    }
}

impl horizon_client::arrows::CanHarmPlayer for Game<'_> {
    /// `owner.canHarmPlayer(target)`, which a server player answers as
    /// `ServerPlayer.canHarmPlayer`: the `pvp` game rule is on and the
    /// shooter's team lets it hurt the target. Only an arrow whose owner and
    /// target are both players reads the rule; a mob on the shooter's team is
    /// still a target.
    fn can_harm_player(&self, owner: Option<Who>, who: Who) -> bool {
        let Some(owner) = owner else {
            return true;
        };
        let (Some(owner_team), Some(target_team)) =
            (self.player_team_of(owner), self.player_team_of(who))
        else {
            return true;
        };
        self.player_may_harm(owner_team, target_team)
    }
}

#[cfg(test)]
mod can_harm_player_tests {
    use super::*;
    use crate::game::game_tests::tests::server_game;
    use horizon_client::arrows::CanHarmPlayer;
    use horizon_client::gamerules::Value;

    /// `ServerPlayer.canHarmPlayer` reads the `pvp` game rule: with it off, a
    /// player's arrow passes through a player (here the shooter itself, once
    /// the arrow has left it).
    #[test]
    #[ignore = "daily"]
    fn an_arrow_between_players_follows_the_pvp_rule() {
        let mut resources = None;
        let mut game = server_game(&mut resources);
        assert!(game.ui.rules.set("pvp", Value::Bool(false)));
        assert!(!game.can_harm_player(Some(Who::Player), Who::Player));
        assert!(game.ui.rules.set("pvp", Value::Bool(true)));
        assert!(game.can_harm_player(Some(Who::Player), Who::Player));
    }
}

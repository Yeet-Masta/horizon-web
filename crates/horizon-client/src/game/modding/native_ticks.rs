//! Entities ticking for native mods (`native_entity_tick_pre`, `native_entity_tick_post`:
//! NeoForge's `EntityTickEvent.Pre` and `.Post`). A family of entities ticks in its own loop,
//! and the loops of the mobs and the items hold their lists out of `Game`, so a family hands
//! its ticking entities to a `TickRound` before the loop and hears the Posts after it; the
//! minecarts and boats, whose loops can call the hooks, ask for each one in turn.
//!
//! Each entity's position writes are filed for `entity_entering_section`
//! (`entity_sections.rs`) while its tick is still running: a round keeps its entities while
//! either hook is listened to.
use super::Game;
use horizon_client::mobs::MobType;
use std::sync::Arc;

/// The type of an entity in a round, as the hooks name it.
pub(super) enum TickKind {
    Fixed(&'static str),
    Mob(Arc<MobType>),
}

impl TickKind {
    fn id(&self) -> &str {
        match self {
            Self::Fixed(id) => id,
            Self::Mob(kind) => &kind.id,
        }
    }
}

/// The entities of one family in one tick.
#[derive(Default)]
pub(in crate::game) struct TickRound {
    /// Whether this game is a server the tick hooks hear: otherwise no Pre or Post is told.
    listening: bool,
    /// Whether `entity_entering_section` is listened to on this server: each entity's position
    /// write is filed and told while its tick is still running.
    sections: bool,
    /// Whether any mod requested per-entity Pre/Post delivery for at least one entity type.
    each_any: bool,
    /// Whether any mod requested per-entity Pre/Post delivery for every entity type.
    each_all: bool,
    /// The entities the Pre let through, in order, for the Post.
    ticking: Vec<(u32, TickKind)>,
    /// The entities a Pre canceled.
    canceled: Vec<u32>,
    /// The items whose part ended their tick (`on_entity_item_update`): they are not
    /// canceled, so they still hear their Post.
    halted: Vec<u32>,
}

impl TickRound {
    /// Whether a mod canceled the tick of entity `id`.
    pub(super) fn canceled(&self, id: u32) -> bool {
        !self.canceled.is_empty() && self.canceled.contains(&id)
    }
    /// Record that a per-entity Pre hook canceled entity `id`.
    pub(super) fn cancel(&mut self, id: u32) {
        self.canceled.push(id);
    }
    /// Whether entity type `kind` requested per-entity Pre/Post delivery right around its
    /// own tick (`r.entity_tick_each(kind)` or `r.entity_tick_each_entity()`).
    pub(super) fn each(&self, kind: &str) -> bool {
        self.listening
            && (self.each_all || (self.each_any && mod_api::__private::entity_tick_each_for(kind)))
    }
    /// Whether a part ended the tick of item `id` (`on_entity_item_update`).
    pub(super) fn halted(&self, id: u32) -> bool {
        !self.halted.is_empty() && self.halted.contains(&id)
    }
    /// A part ended the tick of item `id`.
    pub(super) fn halt(&mut self, id: u32) {
        self.halted.push(id);
    }
    /// Whether `entity_entering_section` is listened to: each entity's position updates are filed.
    pub(super) fn sections(&self) -> bool {
        self.sections
    }
    /// Whether the round keeps the entities that tick: a mod hears their ticks or their moves.
    pub(super) fn listening(&self) -> bool {
        self.listening || self.sections
    }
    /// The entity `id`, told its Pre, went before it ticked (an item merged into an earlier
    /// one): it hears no Post.
    pub(super) fn not_ticked(&mut self, id: u32) {
        if self.listening() {
            self.ticking.retain(|(other, _)| *other != id);
        }
    }
}

impl Game<'_> {
    /// A round for a family about to tick: on a server only (the server thread or a dedicated
    /// server, never a client), and only while a mod listens
    /// (`native_entity_tick_listened`). Its list is kept between ticks.
    pub(super) fn native_tick_round(&mut self) -> TickRound {
        let server = self.serving.is_some() || self.options.dedicated.is_some();
        let mut listened = false;
        if server {
            self.native_entity_tick_listened(&mut listened);
        }
        let listening = server && listened;
        let sections = self.is_logical_server() && self.entity_section_scope().is_some();
        let each_any = listening && mod_api::__private::entity_tick_each_any();
        let each_all = each_any && mod_api::__private::entity_tick_each_all();
        TickRound {
            listening,
            sections,
            each_any,
            each_all,
            ticking: if listening || sections {
                std::mem::take(&mut self.creatures.tick_buffer)
            } else {
                Vec::new()
            },
            canceled: Vec::new(),
            halted: Vec::new(),
        }
    }

    /// `native_entity_tick_pre` for the entity `id` of `kind` about to tick in a family that
    /// hears its Posts together: whether it ticks.
    pub(super) fn native_tick_pre_batch(
        &mut self,
        round: &mut TickRound,
        id: u32,
        kind: TickKind,
    ) -> bool {
        if !round.listening() {
            return true;
        }
        let mut canceled = false;
        if round.listening {
            self.native_entity_tick_pre(self.dim.id(), id, kind.id(), &mut canceled);
        }
        if canceled {
            round.canceled.push(id);
        } else {
            round.ticking.push((id, kind));
        }
        !canceled
    }

    /// `native_entity_tick_post` for each entity the round let through after its position writes
    /// have already filed and told their crossings.
    pub(super) fn native_tick_post_batch(&mut self, mut round: TickRound) {
        if !round.listening() {
            return;
        }
        let dimension = self.dim.id();
        for (id, kind) in &round.ticking {
            if round.listening {
                self.native_entity_tick_post(dimension, *id, kind.id());
            }
        }
        round.ticking.clear();
        self.creatures.tick_buffer = round.ticking;
    }

    /// `native_entity_tick_pre` for one entity about to tick: whether it does.
    pub(super) fn native_tick_pre_one(
        &mut self,
        round: &mut TickRound,
        id: u32,
        kind: &str,
    ) -> bool {
        if !round.listening {
            return true;
        }
        let mut canceled = false;
        self.native_entity_tick_pre(self.dim.id(), id, kind, &mut canceled);
        if canceled {
            round.canceled.push(id);
        }
        !canceled
    }

    /// `native_entity_tick_post` for one entity that ticked, after its move was filed and told.
    pub(super) fn native_tick_post_one(&mut self, round: &TickRound, id: u32, kind: &str) {
        if round.listening {
            self.native_entity_tick_post(self.dim.id(), id, kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round() -> TickRound {
        TickRound {
            listening: true,
            ..TickRound::default()
        }
    }

    /// A canceled entity is skipped, and one that went before its tick (an item merged into an
    /// earlier one) hears no Post; a round nobody listens to keeps nothing.
    #[test]
    fn a_round_keeps_the_entities_that_tick() {
        let mut r = round();
        r.ticking.push((1, TickKind::Fixed("minecraft:item")));
        r.ticking.push((2, TickKind::Fixed("minecraft:item")));
        r.canceled.push(3);
        assert!(r.canceled(3));
        assert!(!r.canceled(1));
        r.not_ticked(2);
        let left: Vec<_> = r
            .ticking
            .iter()
            .map(|(id, kind)| (*id, kind.id()))
            .collect();
        assert_eq!(left, [(1, "minecraft:item")]);
        let mut quiet = TickRound::default();
        assert!(!quiet.listening());
        quiet.not_ticked(1);
        assert!(quiet.ticking.is_empty() && !quiet.canceled(1));
    }

    #[test]
    fn entity_tick_each_fires_pre_and_post_around_each_listed_mob_in_order() {
        if horizon_std::env::var_os("HORIZON_TEST_RESOURCES").is_none() {
            return;
        }
        use crate::game::api_values::install_parts;
        use horizon_client::chunk::Chunk;
        use horizon_client::mobs::Mob;
        use horizon_core::geometry::Vec3;
        use horizon_std::sync::Mutex;
        use mod_api::{
            EntityTickListenedEvent, EntityTickPostEvent, EntityTickPreEvent, Flow, Hooks,
            ModEntry, Mods, Registrar, RegistrationContext, ServerHost,
        };
        use std::sync::Arc;

        struct OrderMod {
            all: bool,
            log: Arc<Mutex<Vec<(u32, &'static str)>>>,
        }
        #[mod_api::__subscriptions]
        impl Hooks for OrderMod {
            fn register(&self, r: &mut dyn Registrar, _cx: &RegistrationContext) {
                if self.all {
                    r.entity_tick_each_entity();
                } else {
                    r.entity_tick_each("minecraft:zombie");
                }
            }
            fn entity_tick_listened(
                &self,
                _host: &mut dyn ServerHost,
                event: &mut EntityTickListenedEvent,
            ) {
                event.listened = true;
            }
            fn entity_tick_pre(
                &self,
                _host: &mut dyn ServerHost,
                event: &mut EntityTickPreEvent<'_>,
            ) -> Flow {
                self.log.lock().unwrap().push((event.entity, "pre"));
                Flow::Continue
            }
            fn entity_tick_post(
                &self,
                _host: &mut dyn ServerHost,
                event: &mut EntityTickPostEvent<'_>,
            ) {
                self.log.lock().unwrap().push((event.entity, "post"));
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let mods: Mods = vec![ModEntry::new(
            "order_test",
            Box::new(OrderMod {
                all: false,
                log: log.clone(),
            }),
        )]
        .into();
        mod_api::testing::with_parts_scope(|| {
            mod_platform::testing::with_mods(mods.clone(), || {
                let mut game = crate::game::native_machines::tests::server_game();
                // The rounds run only on an authoritative server.
                game.options.dedicated = Some(Vec::new());
                install_parts(&mods).unwrap();
                assert!(mod_api::__private::entity_tick_each_any());
                assert!(!mod_api::__private::entity_tick_each_all());
                assert!(mod_api::__private::entity_tick_each_for("minecraft:zombie"));
                assert!(!mod_api::__private::entity_tick_each_for("minecraft:cow"));

                game.world
                    .chunks
                    .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
                let zombie = game
                    .res
                    .mob_types()
                    .get("minecraft:zombie")
                    .unwrap()
                    .clone();
                game.mobs.push(Mob::new(
                    201,
                    zombie.clone(),
                    Vec3::new(2.0, 64.0, 2.0),
                    0.0,
                ));
                game.mobs
                    .push(Mob::new(202, zombie, Vec3::new(4.0, 64.0, 2.0), 0.0));
                for m in game.mobs.iter_mut() {
                    m.invulnerable_time = 5;
                }
                game.tick_creatures().unwrap();

                // Both mobs ran their whole tick (its first step counts this down),
                // not a resumed half-tick after the Pre or Post yield.
                for m in game.mobs.iter() {
                    assert_eq!(m.invulnerable_time, 4, "mob {} did not tick", m.id);
                }

                assert_eq!(
                    *log.lock().unwrap(),
                    vec![(201, "pre"), (201, "post"), (202, "pre"), (202, "post")]
                );
            });
        });
    }

    #[test]
    fn entity_tick_each_fires_post_when_burning_item_dies_on_early_exit() {
        if horizon_std::env::var_os("HORIZON_TEST_RESOURCES").is_none() {
            return;
        }
        use crate::game::api_values::install_parts;
        use horizon_client::chunk::Chunk;
        use horizon_client::entities::ItemEntity;
        use horizon_client::items::ItemStack;
        use horizon_core::geometry::Vec3;
        use horizon_std::sync::Mutex;
        use mod_api::{
            EntityTickListenedEvent, EntityTickPostEvent, EntityTickPreEvent, Flow, Hooks,
            ModEntry, Mods, Registrar, RegistrationContext, ServerHost,
        };
        use std::sync::Arc;

        struct ItemOrderMod {
            log: Arc<Mutex<Vec<(u32, &'static str)>>>,
        }
        #[mod_api::__subscriptions]
        impl Hooks for ItemOrderMod {
            fn register(&self, r: &mut dyn Registrar, _cx: &RegistrationContext) {
                r.entity_tick_each("minecraft:item");
            }
            fn entity_tick_listened(
                &self,
                _host: &mut dyn ServerHost,
                event: &mut EntityTickListenedEvent,
            ) {
                event.listened = true;
            }
            fn entity_tick_pre(
                &self,
                _host: &mut dyn ServerHost,
                event: &mut EntityTickPreEvent<'_>,
            ) -> Flow {
                self.log.lock().unwrap().push((event.entity, "pre"));
                Flow::Continue
            }
            fn entity_tick_post(
                &self,
                _host: &mut dyn ServerHost,
                event: &mut EntityTickPostEvent<'_>,
            ) {
                self.log.lock().unwrap().push((event.entity, "post"));
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let mods: Mods = vec![ModEntry::new(
            "item_burn_post_test",
            Box::new(ItemOrderMod { log: log.clone() }),
        )]
        .into();
        mod_api::testing::with_parts_scope(|| {
            mod_platform::testing::with_mods(mods.clone(), || {
                let mut game = crate::game::native_machines::tests::server_game();
                // The rounds run only on an authoritative server.
                game.options.dedicated = Some(Vec::new());
                install_parts(&mods).unwrap();
                game.world
                    .chunks
                    .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
                let mut item = ItemEntity::new(
                    301,
                    ItemStack::new("minecraft:stick", 1),
                    Vec3::new(2.0, 64.0, 2.0),
                    Vec3::ZERO,
                    0.0,
                );
                item.health = 1;
                item.fire_ticks = 20;
                game.dropped.push(item);

                game.tick_items();

                assert!(
                    game.dropped.iter().all(|it| it.id != 301),
                    "burning item should be destroyed"
                );
                assert_eq!(*log.lock().unwrap(), vec![(301, "pre"), (301, "post")]);
            });
        });
    }
}

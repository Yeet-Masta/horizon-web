#[cfg(test)]
mod tests {
    use mod_api::{EntityDamagedDetailEvent, Hooks, ModEntry, ServerHost};
    use std::sync::{Arc, Mutex};

    struct Probe(Arc<Mutex<Vec<(u32, String, String, u32)>>>);
    #[mod_api::__subscriptions]
    impl Hooks for Probe {
        fn entity_damaged_detail(
            &self,
            _: &mut dyn ServerHost,
            event: &mut EntityDamagedDetailEvent<'_>,
        ) {
            self.0.lock().unwrap().push((
                event.entity,
                event.kind.to_string(),
                event.damage_type.to_string(),
                event.causing,
            ));
        }
    }

    /// A hit on an armor stand names its type and damage.
    #[test]
    #[ignore = "daily"]
    fn a_hit_on_an_armor_stand_names_its_type() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mods: mod_api::Mods = vec![ModEntry::new(
            "damage-probe",
            Box::new(Probe(Arc::clone(&heard))),
        )]
        .into();
        mod_platform::testing::with_mods(mods, || {
            crate::game::native_guest_turn_tests::with_guests(|game| {
                game.stand_damaged(41, "minecraft:explosion", 7);
                assert_eq!(
                    *heard.lock().unwrap(),
                    [(
                        41,
                        "minecraft:armor_stand".to_owned(),
                        "minecraft:explosion".to_owned(),
                        7
                    )]
                );
            });
        });
    }
}

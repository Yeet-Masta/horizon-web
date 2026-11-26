#[cfg(test)]
mod tests {
    use horizon_std::sync::Mutex;
    use mod_api::{Hooks, MobEffectAddedDetailEvent, ModEntry, ServerHost};
    use std::sync::Arc;

    struct Probe(Arc<Mutex<Vec<(String, Option<i32>, i32, i32, bool, String)>>>);
    #[mod_api::__subscriptions]
    impl Hooks for Probe {
        fn mob_effect_added_detail(
            &self,
            _: &mut dyn ServerHost,
            event: &mut MobEffectAddedDetailEvent<'_>,
        ) {
            self.0.lock().unwrap().push((
                event.effect.to_string(),
                event.entity,
                event.amplifier,
                event.duration,
                event.new,
                event.cause.to_string(),
            ));
        }
    }

    /// A vanilla effect is heard with its duration and cause; a client hears nothing.
    #[test]
    #[ignore = "daily"]
    fn a_vanilla_effect_is_heard_with_duration_and_cause() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mods: mod_api::Mods = vec![ModEntry::new(
            "effect-probe",
            Box::new(Probe(Arc::clone(&heard))),
        )]
        .into();
        mod_platform::testing::with_mods(mods, || {
            crate::game::native_guest_turn_tests::with_guests(|game| {
                game.effect_added_detail("minecraft:bad_omen", Some(7), 1, 120000, true, "command");
                game.effect_added_detail("minecraft:speed", None, 0, -1, false, "unknown");
                // A player's effect id is a bare path: it is reported namespaced.
                game.effect_added_detail("haste", None, 0, 20, true, "plugin");
                assert_eq!(
                    *heard.lock().unwrap(),
                    [
                        (
                            "minecraft:bad_omen".to_owned(),
                            Some(7),
                            1,
                            120000,
                            true,
                            "command".to_owned()
                        ),
                        (
                            "minecraft:speed".to_owned(),
                            None,
                            0,
                            -1,
                            false,
                            "unknown".to_owned()
                        ),
                        (
                            "minecraft:haste".to_owned(),
                            None,
                            0,
                            20,
                            true,
                            "plugin".to_owned()
                        ),
                    ]
                );
            });
        });
    }
}

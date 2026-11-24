#[cfg(test)]
mod tests {
    use mod_api::{Hooks, ModEntry, PlayerTeleportCauseEvent, ServerHost};
    use std::sync::{Arc, Mutex};

    struct Probe(Arc<Mutex<Vec<(u128, [f64; 3], [f64; 3], String, String)>>>);
    #[mod_api::__subscriptions]
    impl Hooks for Probe {
        fn player_teleport_cause(
            &self,
            _: &mut dyn ServerHost,
            event: &mut PlayerTeleportCauseEvent<'_>,
        ) {
            self.0.lock().unwrap().push((
                event.player,
                event.from,
                event.to,
                event.dimension.to_string(),
                event.cause.to_string(),
            ));
        }
    }

    /// A command teleport is told with its cause, along with `player_teleported`.
    #[test]
    #[ignore = "daily"]
    fn a_command_teleport_is_told_with_its_cause() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mods: mod_api::Mods = vec![ModEntry::new(
            "tp-probe",
            Box::new(Probe(Arc::clone(&heard))),
        )]
        .into();
        mod_platform::testing::with_mods(mods, || {
            crate::game::native_guest_turn_tests::with_guests(|game| {
                game.teleported_because(
                    13,
                    "guest13",
                    [0.0, 64.0, 0.0],
                    [10.0, 70.0, -4.0],
                    "minecraft:overworld",
                    "command",
                );
                assert_eq!(
                    *heard.lock().unwrap(),
                    [(
                        13,
                        [0.0, 64.0, 0.0],
                        [10.0, 70.0, -4.0],
                        "minecraft:overworld".to_owned(),
                        "command".to_owned()
                    )]
                );
            });
        });
    }

    /// The acting server player's pearl and chorus teleports carry their own causes.
    #[test]
    #[ignore = "daily"]
    fn a_pearl_teleport_is_told_with_its_cause() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mods: mod_api::Mods = vec![ModEntry::new(
            "tp-probe",
            Box::new(Probe(Arc::clone(&heard))),
        )]
        .into();
        mod_platform::testing::with_mods(mods, || {
            crate::game::native_entity_refs::tests::with_game(|game| {
                game.lan_acting_uuid = Some(9);
                game.player = Some(
                    horizon_client::motion::PlayerMotion::new(
                        game.motion,
                        horizon_core::geometry::Vec3::ZERO,
                    )
                    .unwrap(),
                );
                game.acting_teleport_cause([1.0, 2.0, 3.0], "ender_pearl");
                let heard = heard.lock().unwrap();
                assert_eq!(heard.len(), 1);
                assert_eq!(
                    (heard[0].0, heard[0].1, heard[0].4.as_str()),
                    (9, [1.0, 2.0, 3.0], "ender_pearl")
                );
            });
        });
    }
}

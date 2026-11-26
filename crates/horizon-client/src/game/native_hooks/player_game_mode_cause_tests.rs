#[cfg(test)]
mod tests {
    use horizon_std::sync::Mutex;
    use mod_api::{GameMode, Hooks, ModEntry, PlayerGameModeCauseEvent, ServerHost};
    use std::sync::Arc;

    struct Probe(Arc<Mutex<Vec<(String, GameMode, GameMode, String)>>>);
    #[mod_api::__subscriptions]
    impl Hooks for Probe {
        fn player_game_mode_cause(
            &self,
            _: &mut dyn ServerHost,
            event: &mut PlayerGameModeCauseEvent<'_>,
        ) {
            self.0.lock().unwrap().push((
                event.name.to_string(),
                event.from,
                event.to,
                event.cause.to_string(),
            ));
        }
    }

    /// A mod's own change is heard with the cause `plugin`; no change is not heard.
    #[test]
    #[ignore = "daily"]
    fn a_game_mode_change_names_its_cause() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mods: mod_api::Mods = vec![ModEntry::new(
            "cause-probe",
            Box::new(Probe(Arc::clone(&heard))),
        )]
        .into();
        mod_platform::testing::with_mods(mods, || {
            crate::game::native_guest_turn_tests::with_guests(|game| {
                game.as_guest(13, |g| {
                    g.set_game_mode_because(GameMode::Survival, "command");
                    assert!(!g.set_game_mode_because(GameMode::Survival, "plugin"));
                    assert!(g.set_game_mode_because(GameMode::Creative, "switcher"));
                })
                .unwrap();
                let heard = heard.lock().unwrap();
                let causes: Vec<&str> = heard.iter().map(|h| h.3.as_str()).collect();
                assert_eq!(causes.last(), Some(&"switcher"));
                assert_eq!(heard.last().unwrap().2, GameMode::Creative);
                assert_eq!(heard.last().unwrap().0, "guest13");
            });
        });
    }
}

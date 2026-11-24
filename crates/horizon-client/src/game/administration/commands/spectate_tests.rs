//! `/spectate <target> <player>` checks whether the named player is in
//! spectator mode, not the command's source (#20730).
use crate::game::native_entity_refs::tests::with_game;
use horizon_client::command::args::GameMode;
use horizon_client::lan::{Joined, NetConfig, PlayerSender, guest::Guest, hub::HubConfig};
use horizon_protocol::packets::configuration::ClientInformation;
use std::sync::{Arc, atomic::AtomicI32, mpsc};

#[test]
#[ignore = "daily"]
fn spectate_checks_named_player_mode_not_source() {
    with_game(|game| {
        game.build_registries();
        game.set_allow_commands(true);

        game.lan_host(
            crate::game::lan::LanSettings {
                port: 0,
                guest_commands: true,
                force_game_mode: false,
            },
            NetConfig {
                listen: false,
                announce: false,
                ..NetConfig::loopback(0, String::new(), 2)
            },
            HubConfig {
                default_game_mode: 0,
                guest_commands: true,
                force_game_mode: false,
                hardcore: false,
                view_distance: 2,
                simulation_distance: 2,
                dimension: "minecraft:overworld".into(),
                allow_flight: false,
                seed: 0,
                max_players: 20,
                integrated: true,
                levels: Default::default(),
                level_seeds: Default::default(),
            },
        )
        .unwrap();

        let (tx, _rx) = mpsc::channel();
        let mut guest = Guest::new(
            Joined {
                conn: 1,
                uuid: 1,
                name: "Alex".into(),
                properties: Vec::new(),
                info: ClientInformation::new(2),
                brand: String::new(),
                sender: PlayerSender::new(tx, Arc::default()),
                latency: Arc::new(AtomicI32::new(0)),
                address: "127.0.0.1:1".parse().unwrap(),
                expected: false,
            },
            500,
            [0.5, 80.0, 0.5],
            2,
        );
        guest.placed = true;
        guest.dimension = "minecraft:overworld".into();
        guest.game_mode = 3; // Spectator
        game.lan.as_mut().unwrap().hub.guests.push(guest);

        // Source player is in creative mode (not spectator).
        game.set_mode(GameMode::Creative);
        assert!(!game.spectator());

        // Summon pig as target.
        assert!(game.run_command(
            "summon minecraft:pig 8.5 100.0 8.5 {UUID:[I;73,7,9,11],NoAI:1b,NoGravity:1b}"
        ));

        // Creative source spectating with spectator guest Alex succeeds (#20730).
        assert!(
            game.run_command("spectate @e[type=minecraft:pig,limit=1] Alex"),
            "spectating with a spectator player target succeeds even if source is creative"
        );

        // Now change Alex to Survival (0).
        game.lan.as_mut().unwrap().hub.guests.update(0, |guest| {
            guest.game_mode = 0; // Survival
        });

        // Even if source sets its mode to Spectator, named player Alex in Survival must fail.
        game.set_mode(GameMode::Spectator);
        assert!(game.spectator());
        assert!(
            !game.run_command("spectate @e[type=minecraft:pig,limit=1] Alex"),
            "spectating with a non-spectator player target fails even if source is spectator"
        );

        // An acting guest in spectator mode is evaluated live in self (#21019).
        game.lan_acting = Some(1);
        game.set_mode(GameMode::Spectator);
        assert!(game.target_spectator(super::super::Target::Guest(1)));
        assert!(game.target_spectator(super::super::Target::Player));

        // When in survival mode, it evaluates to false.
        game.set_mode(GameMode::Survival);
        assert!(!game.target_spectator(super::super::Target::Guest(1)));
        assert!(!game.target_spectator(super::super::Target::Player));
        game.lan_acting = None;
    });
}

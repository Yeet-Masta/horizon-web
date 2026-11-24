//! `/playsound`: `PlaySoundCommand` plays for a source's player, and its
//! packet's seed is what every client picks the variant from
//! (`ClientLevel.playSeededSound`), one packet per target.
use super::*;
use crate::game::commands::execute::Source;
use crate::game::{
    Audio, Options, Progress, Settings, execution_role::ExecutionRole, load_resources,
};
use horizon_client::{
    chunk::Chunk,
    lan::{Joined, NetConfig, Out, PlayerSender, guest::Guest, hub::HubConfig},
    motion::PlayerMotion,
};
use horizon_core::{
    geometry::Vec3,
    random::{LegacyRandom, RandomSource},
};
use horizon_protocol::packets::configuration::ClientInformation;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, atomic::AtomicI32, mpsc};

fn with_game(test: impl FnOnce(&mut Game<'_>)) {
    let root = std::path::PathBuf::from(
        std::env::var_os("HORIZON_TEST_RESOURCES").expect("prepared resources"),
    );
    let settings = Settings::default();
    let loaded = load_resources(
        horizon_assets::resource_source::ResourceRoot::directory(root.clone()),
        Vec::new(),
        None,
        &root,
        &settings,
        std::time::Instant::now(),
        false,
        ExecutionRole::IntegratedServer,
        &Progress::new(),
        &mut |_, _| unreachable!(),
    )
    .unwrap();
    let mut options = Options::parse(std::iter::empty()).unwrap().unwrap();
    options.resources = root.clone();
    options.game_dir = Some(
        std::path::PathBuf::from(std::env::var_os("HORIZON_HOME").expect("HORIZON_HOME"))
            .join(format!("playsound-{:?}", std::thread::current().id())),
    );
    options.execution_role = ExecutionRole::IntegratedServer;
    let res: &'static _ = Box::leak(Box::new(loaded.resources));
    let mut game = Game::new(
        res,
        loaded.pack,
        loaded.gui.unwrap(),
        options,
        settings,
        Audio::silent(&horizon_assets::resource_source::ResourceSource::directory(
            root.to_path_buf(),
        )),
        None,
    )
    .unwrap();
    game.build_registries();
    game.set_allow_commands(true);
    game.world
        .chunks
        .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    game.player = Some(PlayerMotion::new(game.motion, Vec3::new(4.5, 200.0, 4.5)).unwrap());
    test(&mut game);
}

/// `PlaySoundCommand`: without `targets` it plays for
/// `getCallingPlayerAsCollection(source.getPlayer())`, empty for a command
/// block or the console, so the command fails and nothing is heard.
#[test]
#[ignore = "daily"]
fn a_source_without_a_player_fails_and_plays_nothing() {
    with_game(|game| {
        game.command_source = Some(Source {
            pos: [4.5, 200.0, 4.5],
            silent: false,
            ..game.server_source()
        });
        game.chat_capture = Some(Vec::new());
        assert!(!game.run_command("playsound minecraft:entity.pig.ambient master"));
        let failure = game.chat_capture.take().unwrap();
        assert_eq!(failure.len(), 1);
        assert_eq!(
            failure[0].plain(),
            horizon_client::lang::get("commands.playsound.failed")
        );
        assert_eq!(game.audio.heard.get("minecraft:entity.pig.ambient"), None);
        // A command block's source has no entity either.
        game.command_source = Some(Source {
            pos: [4.5, 200.0, 4.5],
            silent: false,
            block: Some((4, 200, 4)),
            ..game.server_source()
        });
        assert!(!game.run_command("playsound minecraft:entity.pig.ambient master"));
        assert_eq!(game.audio.heard.get("minecraft:entity.pig.ambient"), None);
        // The player's own source still names them.
        game.command_source = None;
        assert!(game.run_command("playsound minecraft:entity.pig.ambient master"));
        assert_eq!(
            game.audio.heard.get("minecraft:entity.pig.ambient"),
            Some(&1)
        );
    });
}

/// `ClientPacketListener.handleSoundEvent` plays the packet's sound seeded by
/// `packet.getSeed()`, so every client picks the same variant:
/// `WeighedSoundEvents.getSound` draws one `nextInt(total weight)`.
#[test]
#[ignore = "daily"]
fn a_sound_packet_picks_its_variant_from_the_packet_s_seed() {
    with_game(|game| {
        game.audio.engine.registry.add_file(
            "fixture",
            br#"{"pick":{"sounds":[{"name":"fixture:a","weight":3},{"name":"fixture:b","weight":1}]}}"#,
            &|_, _| true,
        );
        game.audio.engine.loaded = true;
        let seed = 0x0BAD_C0DE_1234_5678u64 as i64;
        game.handle_packet(C::Sound(play::Sound {
            sound: (-1, Some(("fixture:pick".into(), None))),
            source: 0,
            x: 0,
            y: 0,
            z: 0,
            volume: 1.0,
            pitch: 1.0,
            seed,
        }))
        .unwrap();
        let instance = (1..=16)
            .filter_map(|id| game.audio.engine.instance(id))
            .find(|i| i.identifier == "fixture:pick")
            .expect("the packet's sound reached the engine");
        let mut random = LegacyRandom::new(seed);
        let index = random.next_int_bound(4).unwrap();
        let expected = if index < 3 { "fixture:a" } else { "fixture:b" };
        assert_eq!(
            instance.sound.as_ref().map(|s| s.location.as_str()),
            Some(expected),
            "the variant vanilla picks from the packet's seed"
        );
        let mut after = instance.random.clone();
        assert_eq!(
            after.next_long(),
            random.next_long(),
            "the instance's random is the packet's seed, not the client's"
        );
    });
}

/// One placed guest of a hosted world, and what its client is sent.
fn lan_guest(game: &mut Game<'_>, conn: u32, name: &str, position: [f64; 3]) -> Receiver<Out> {
    game.world_root = game
        .options
        .game_dir
        .as_ref()
        .unwrap()
        .join("playsound-world");
    std::fs::create_dir_all(&game.world_root).unwrap();
    game.lan_host(
        crate::game::lan::LanSettings {
            port: 0,
            guest_commands: true,
            force_game_mode: false,
        },
        NetConfig {
            announce: false,
            ..NetConfig::loopback(0, "playsound-test".into(), 2)
        },
        HubConfig {
            default_game_mode: 1,
            guest_commands: true,
            force_game_mode: false,
            hardcore: false,
            view_distance: 2,
            simulation_distance: 2,
            dimension: "minecraft:overworld".into(),
            allow_flight: false,
            seed: 0,
            max_players: 3,
            integrated: false,
            levels: Default::default(),
            level_seeds: Default::default(),
        },
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let mut guest = Guest::new(
        Joined {
            conn,
            uuid: horizon_protocol::crypto::offline_uuid(name),
            name: name.into(),
            properties: Vec::new(),
            info: ClientInformation::new(2),
            brand: "test".into(),
            sender: PlayerSender::new(tx, Arc::default()),
            latency: Arc::new(AtomicI32::new(0)),
            address: "127.0.0.1:25565".parse().unwrap(),
            expected: false,
        },
        conn as i32,
        position,
        2,
    );
    guest.dimension = "minecraft:overworld".into();
    guest.placed = true;
    guest.loaded = true;
    guest.game_mode = 1;
    game.lan.as_mut().unwrap().hub.guests.push(guest);
    rx
}

/// Every sound packet a guest was sent, as the client plays them.
fn sent_sounds(rx: &Receiver<Out>) -> Vec<play::Sound> {
    let mut sounds = Vec::new();
    for out in rx.try_iter() {
        let bodies = match out {
            Out::Packet(body) => vec![body],
            Out::Packets(more) => more,
            Out::Disconnect(_) => panic!("the guest was disconnected"),
        };
        for body in bodies {
            let mut reader = horizon_protocol::buf::Reader::new(&body);
            let id = reader.varint().unwrap();
            if let C::Sound(p) = C::decode(id, reader.rest()).unwrap() {
                sounds.push(p);
            }
        }
    }
    sounds
}

/// `PlaySoundCommand.playSound` sends one packet per target; the host's own
/// play is not captured and broadcast to the guests again.
#[test]
#[ignore = "daily"]
fn a_targeted_guest_hears_a_played_sound_once() {
    with_game(|game| {
        let rx = lan_guest(game, 2, "Bob", [6.5, 200.0, 4.5]);
        assert!(
            game.run_command("playsound minecraft:entity.pig.ambient master @a 4.5 200.0 4.5 1 1")
        );
        game.lan_broadcast_effects();
        // The host's own player is a target too and still hears it once.
        assert_eq!(
            game.audio.heard.get("minecraft:entity.pig.ambient"),
            Some(&1),
            "the host's own play is not dropped"
        );
        let sounds = sent_sounds(&rx);
        assert_eq!(sounds.len(), 1, "one packet per target");
        assert_eq!(
            (sounds[0].x, sounds[0].y, sounds[0].z),
            (36, 1600, 36),
            "at the command's position"
        );
    });
}

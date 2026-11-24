//! `player_send_resource_pack`, `player_set_view_distance` and `log_file` against real LAN guests.

use super::native_guest_turn_tests::with_guests;
use horizon_protocol::packets::play::Clientbound as C;

/// The pack the guest is offered, and each refusal that sends nothing.
#[test]
#[ignore = "daily"]
fn a_mod_offers_one_player_a_resource_pack() {
    with_guests(|game| {
        let b = game.player(13).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        game.lan
            .as_mut()
            .unwrap()
            .hub
            .guests
            .update_connection(13, |g| {
                g.sender = horizon_client::lan::PlayerSender::new(tx, Default::default());
            });
        let sha1 = "0123456789abcdef0123456789ABCDEF01234567";
        assert!(game.player_send_resource_pack(b, "https://example.org/a.zip", sha1, true, Some("Pack")));
        assert!(game.player_send_resource_pack(b, "https://example.org/b.zip", "", false, None));
        assert!(!game.player_send_resource_pack(b, "", sha1, true, None), "no url");
        assert!(!game.player_send_resource_pack(b, "https://example.org/a.zip", "abc", true, None), "short hash");
        assert!(!game.player_send_resource_pack(b, "https://example.org/a.zip", &"g".repeat(40), true, None), "not hex");
        assert!(!game.player_send_resource_pack(b, &"u".repeat(32768), sha1, true, None), "url too long");
        let mut sent = Vec::new();
        while let Ok(horizon_client::lan::Out::Packet(body)) = rx.try_recv() {
            let mut r = horizon_protocol::buf::Reader::new(&body);
            let id = r.varint().unwrap();
            if let Ok(C::ResourcePackPush(p)) = C::decode(id, r.rest()) {
                sent.push((p.url, p.hash, p.required, p.prompt.is_some(), p.id));
            }
        }
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0],
            (
                "https://example.org/a.zip".to_owned(),
                sha1.to_owned(),
                true,
                true,
                horizon_client::server::properties::name_uuid(b"https://example.org/a.zip"),
            )
        );
        assert_eq!((sent[1].1.as_str(), sent[1].2, sent[1].3), ("", false, false));
        assert_ne!(sent[0].4, sent[1].4, "each url is its own pack");
    });
}

/// A player's radius is clamped from 2 to the server's view distance; a stale handle is refused.
#[test]
#[ignore = "daily"]
fn a_mod_sets_one_players_view_distance() {
    with_guests(|game| {
        game.lan.as_mut().unwrap().hub.set_view_distance(8);
        let a = game.player(12).unwrap();
        let radius = |game: &super::Game<'_>, conn| game.lan.as_ref().unwrap().hub.guest(conn).unwrap().view_distance;
        assert!(game.player_set_view_distance(a, 5));
        assert_eq!(radius(game, 12), 5);
        assert!(game.player_set_view_distance(a, 1));
        assert_eq!(radius(game, 12), 2);
        assert!(game.player_set_view_distance(a, 99));
        assert_eq!(radius(game, 12), 8);
        assert_eq!(radius(game, 13), 8, "the other player keeps theirs");
        let gone = {
            let entity = a.entity();
            mod_api::__private::game::player_ref(
                mod_api::EntityRef::new(mod_api::WorldEpoch::new(entity.epoch().get() + 1), entity.id()),
                a.uuid(),
            )
        };
        assert!(!game.player_set_view_distance(gone, 4));
        assert_eq!(radius(game, 12), 8);
    });
}

/// The log is `logs/latest.log` under the game's home.
#[test]
#[ignore = "daily"]
fn the_log_file_is_latest_log_in_the_logs_folder() {
    with_guests(|game| {
        let path = game.log_file();
        assert_eq!(path.file_name().unwrap(), "latest.log");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "logs");
    });
}

/// A selector token resolves as the player's own command would: `@a` is both guests, `@s` is the
/// asker, a type with nobody of it is empty, and text that is not one selector is `None`.
#[test]
#[ignore = "daily"]
fn a_mod_resolves_a_selector_for_a_player() {
    with_guests(|game| {
        game.build_registries();
        let (a, b) = (game.player(12).unwrap(), game.player(13).unwrap());
        let ids = |found: Option<Vec<mod_api::EntityRef>>| {
            let mut ids: Vec<u32> = found.unwrap().iter().map(|e| e.id()).collect();
            ids.sort();
            ids
        };
        let mut both = vec![a.entity().id(), b.entity().id()];
        both.sort();
        assert_eq!(ids(game.player_select_entities(a, "@a")), both);
        assert_eq!(ids(game.player_select_entities(b, "@s")), [b.entity().id()]);
        assert_eq!(ids(game.player_select_entities(a, "@e[type=minecraft:zombie]")), Vec::<u32>::new());
        assert!(game.player_select_entities(a, "@a extra").is_none());
        assert!(game.player_select_entities(a, "@z[").is_none());
        assert!(game.player_select_entities(a, "").is_none());
    });
}

/// A player is collidable unless a spectator or on a `never` team.
#[test]
#[ignore = "daily"]
fn a_player_is_collidable_unless_spectator_or_on_a_never_team() {
    with_guests(|game| {
        let a = game.player(12).unwrap();
        assert_eq!(game.player_is_collidable(a), Some(true));
        game.lan.as_mut().unwrap().hub.guests.update_connection(12, |g| g.game_mode = 3);
        assert_eq!(game.player_is_collidable(a), Some(false), "a spectator");
        game.lan.as_mut().unwrap().hub.guests.update_connection(12, |g| g.game_mode = 0);
        game.score.board.add_team("ghosts").collision_rule =
            horizon_client::scoreboard::CollisionRule::Never;
        game.score.board.add_to_team("guest12", "ghosts");
        assert_eq!(game.player_is_collidable(a), Some(false), "a never team");
        game.score.board.team_mut("ghosts").unwrap().collision_rule =
            horizon_client::scoreboard::CollisionRule::Always;
        assert_eq!(game.player_is_collidable(a), Some(true));
    });
}

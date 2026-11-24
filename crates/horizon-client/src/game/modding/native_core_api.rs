//! What a server mod asks of one player's connection and of the server's files: a resource pack
//! offered in play, a player's own view distance, and the path of the server log.
use super::{Game, commands::Target};
use horizon_client::server::{json::Json, properties::name_uuid};
use horizon_protocol::packets::{configuration::ResourcePackPush, play::Clientbound as C};
use mod_api::PlayerRef;
use std::path::PathBuf;

/// The longest url the protocol sends (`Utf8String` of 32767, counted in UTF-16 units as Java's
/// `String.length`).
const MAX_URL_UNITS: usize = 32767;

impl Game<'_> {
    /// `Player.setResourcePack(url, hash, required, prompt)`: a `ClientboundResourcePackPushPacket`
    /// in play, its id the name UUID of the url as the `resource-pack` setting makes it.
    pub(super) fn player_send_resource_pack(
        &mut self,
        player: PlayerRef,
        url: &str,
        sha1: &str,
        required: bool,
        prompt: Option<&str>,
    ) -> bool {
        let hash_fits =
            sha1.is_empty() || (sha1.len() == 40 && sha1.bytes().all(|b| b.is_ascii_hexdigit()));
        if url.is_empty() || url.encode_utf16().count() > MAX_URL_UNITS || !hash_fits {
            return false;
        }
        let Ok(conn) = self.player_connection(player) else {
            return false;
        };
        let Some(guest) = self.lan.as_ref().and_then(|lan| lan.hub.guest(conn)) else {
            return false;
        };
        guest.send(&C::ResourcePackPush(ResourcePackPush {
            id: name_uuid(url.as_bytes()),
            url: url.to_owned(),
            hash: sha1.to_owned(),
            required,
            prompt: prompt.map(|text| {
                horizon_client::lan::conn::component_tag(&Json::String(text.to_owned()))
            }),
        }));
        true
    }
    /// `Player.setViewDistance`: the player's own radius, `Mth.clamp(chunks, 2, serverViewDistance)`
    /// as `ChunkMap.getPlayerViewDistance` clamps what a client asked for. Their chunk centre is
    /// forgotten so the next tick sends or drops the chunks that changed.
    pub(super) fn player_set_view_distance(&mut self, player: PlayerRef, chunks: i32) -> bool {
        let Ok(conn) = self.player_connection(player) else {
            return false;
        };
        let Some(lan) = self.lan.as_mut() else {
            return false;
        };
        let server = lan.hub.config.view_distance;
        lan.hub
            .update_guest(conn, |g| {
                g.view_distance = chunks.max(2).min(server);
                g.center = None;
            })
            .is_some()
    }
    /// The server log being written: `logs/latest.log` under the game's home.
    pub(super) fn log_file(&self) -> PathBuf {
        horizon_client::logging::logs_dir(&horizon_client::logging::game_home()).join("latest.log")
    }
}

impl Game<'_> {
    /// `Server.selectEntities(sender, token)`: the selector `token` (`@a`, `@e[type=villager]`, a
    /// name) resolved as the player's command source would, as they stand now. None for a token
    /// that is not exactly one selector, a client, or a player the game cannot reach.
    pub(super) fn player_select_entities(
        &mut self,
        player: PlayerRef,
        token: &str,
    ) -> Option<Vec<mod_api::EntityRef>> {
        if self.remote.is_some() || token.is_empty() || token.len() > 2048 {
            return None;
        }
        let mut reader = horizon_client::command::Reader::new(token);
        let selector =
            horizon_client::command::selector::parse(&mut reader, &self.ui.registries, true)
                .ok()?;
        if reader.can_read() {
            return None;
        }
        // A fake player has no command source of its own to select from.
        use super::native_server::PlayerRead;
        match self.player_handle_read(player)? {
            PlayerRead::Acting { .. } if self.lan_acting_fake => return None,
            PlayerRead::Guest(guest) if guest.is_fake() => return None,
            _ => {}
        }
        let mut found = None;
        let reached = self.native_as_player_handle(player, |game| {
            // The requested player is the source, not a console or `/execute` one this call came
            // from: no source means the player in place.
            let outer = game.command_source.take();
            let targets = game.select(&selector);
            let home = game.dim;
            let dims = game.loaded_dims();
            let mut entities = Vec::new();
            for t in targets {
                // The selector reaches every loaded level: each target is read in its own.
                let mut entity = game.selected_entity(t);
                for &dim in dims.iter().filter(|&&d| d != home) {
                    if entity.is_some() {
                        break;
                    }
                    let back = game.enter_command_level(dim);
                    entity = game.selected_entity(t);
                    if let Some(back) = back {
                        game.enter_command_level(back);
                    }
                }
                entities.extend(entity);
            }
            game.command_source = outer;
            found = Some(entities);
        });
        found.filter(|_| reached)
    }
}

impl Game<'_> {
    /// The handle of one selected target: a player by their uuid, anything else by its entity id.
    fn selected_entity(&self, target: Target) -> Option<mod_api::EntityRef> {
        match target {
            // The acting guest, else the world's own player (an integrated server's owner has no
            // acting uuid), as `target_by_uuid` reads it.
            Target::Player => {
                let uuid = self.lan_acting_uuid.or_else(|| {
                    self.level.player_uuid.map(|[a, b, c, d]| {
                        (u128::from(a as u32) << 96)
                            | (u128::from(b as u32) << 64)
                            | (u128::from(c as u32) << 32)
                            | u128::from(d as u32)
                    })
                })?;
                self.player(uuid).map(|p| p.entity())
            }
            Target::Guest(conn) => {
                let uuid = self.lan.as_ref()?.hub.guest(conn)?.uuid;
                self.player(uuid).map(|p| p.entity())
            }
            other => self.entity(super::native_entities::target_id(other)?),
        }
    }
}

impl Game<'_> {
    /// `Player.isCollidable` as the entity selector decides who pushes a player
    /// (`EntitySelector.pushableBy`): a spectator never collides, and neither does a player on a
    /// team whose collision rule is `never`. None for a player the game cannot read.
    pub(super) fn player_is_collidable(&self, player: PlayerRef) -> Option<bool> {
        use super::native_server::PlayerRead;
        let mode = self.player_game_mode(player)?;
        let name = match self.player_read_both(player)? {
            PlayerRead::Acting { .. } => self.acting_player_name().to_owned(),
            PlayerRead::Guest(guest) => guest.name.clone(),
        };
        let never = self.score.board.players_team(&name).is_some_and(|team| {
            team.collision_rule == horizon_client::scoreboard::CollisionRule::Never
        });
        Some(mode != mod_api::GameMode::Spectator && !never)
    }
}

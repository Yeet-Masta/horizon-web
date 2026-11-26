use super::*;

impl Game<'_> {
    /// A server player was teleported, with the cause. Server-side.
    pub(in crate::game) fn native_player_teleport_cause(
        &mut self,
        call: mod_api::__private::PlayerTeleportCauseCall<'_>,
    ) {
        self.fire_player_teleport_cause(call);
    }
    /// The acting server player was teleported from `from` to where they stand now.
    pub(in crate::game) fn acting_teleport_cause(&mut self, from: [f64; 3], cause: &str) {
        let Some(uuid) = self.lan_acting_uuid else {
            return;
        };
        let Some(to) = self.player.as_ref().map(|p| [p.position.x, p.position.y, p.position.z])
        else {
            return;
        };
        let name = self.acting_player_name().to_owned();
        let dimension = self.dim.id().to_owned();
        self.native_player_teleport_cause(mod_api::__private::PlayerTeleportCauseCall {
            player: uuid,
            name: &name,
            from,
            to,
            dimension: &dimension,
            cause,
        });
    }
    /// `native_player_teleported`, then `native_player_teleport_cause` with the same teleport.
    pub(in crate::game) fn teleported_because(
        &mut self,
        player: u128,
        name: &str,
        from: [f64; 3],
        to: [f64; 3],
        dimension: &str,
        cause: &str,
    ) {
        self.native_player_teleported(player, name, from, to, dimension);
        self.native_player_teleport_cause(mod_api::__private::PlayerTeleportCauseCall {
            player,
            name,
            from,
            to,
            dimension,
            cause,
        });
    }
}

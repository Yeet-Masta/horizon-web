use super::*;

impl Game<'_> {
    /// A player who joined is about to be placed: the mods may change where. Server-side.
    pub(in crate::game) fn native_player_spawn_location(
        &mut self,
        call: mod_api::__private::PlayerSpawnLocationCall<'_>,
    ) {
        self.fire_player_spawn_location(call);
    }
}

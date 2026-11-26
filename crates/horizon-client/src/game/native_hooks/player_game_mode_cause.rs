use super::*;

impl Game<'_> {
    /// A server player's game mode changed, with the cause. Server-side.
    pub(in crate::game) fn native_player_game_mode_cause(
        &mut self,
        call: mod_api::__private::PlayerGameModeCauseCall<'_>,
    ) {
        self.fire_player_game_mode_cause(call);
    }
}

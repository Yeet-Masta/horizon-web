use super::Game;

impl Game<'_> {
    pub(in crate::game) fn native_controller_move(
        &self,
        call: mod_api::__private::ControllerMoveCall<'_>,
    ) {
        self.fire_controller_move(call);
    }

    pub(in crate::game) fn native_controller_look(
        &self,
        call: mod_api::__private::ControllerLookCall<'_>,
    ) {
        self.fire_controller_look(call);
    }
}

use super::*;

impl Game<'_> {
    /// A server mob or player was given the effect `effect` (any effect, vanilla's included), after
    /// the change. Server-side.
    pub(in crate::game) fn native_mob_effect_added_detail(
        &mut self,
        call: mod_api::__private::MobEffectAddedDetailCall<'_>,
    ) {
        self.fire_mob_effect_added_detail(call);
    }
    /// `native_mob_effect_added_detail` for the game's effect code; nothing on a client.
    pub(in crate::game) fn effect_added_detail(
        &mut self,
        effect: &str,
        entity: Option<i32>,
        amplifier: i32,
        duration: i32,
        new: bool,
        cause: &str,
    ) {
        if self.remote.is_some() {
            return;
        }
        // A player's effect ids are bare registry paths: the hook reports namespaced ones.
        let namespaced;
        let effect = if effect.contains(':') {
            effect
        } else {
            namespaced = format!("minecraft:{effect}");
            &namespaced
        };
        self.native_mob_effect_added_detail(mod_api::__private::MobEffectAddedDetailCall {
            effect,
            entity,
            amplifier,
            duration,
            new,
            cause,
        });
    }
}

use super::*;

impl Game<'_> {
    /// An entity outside `living_damage_post`'s reach (an armor stand) was hit. Server-side.
    pub(in crate::game) fn native_entity_damaged_detail(
        &mut self,
        call: mod_api::__private::EntityDamagedDetailCall<'_>,
    ) {
        self.fire_entity_damaged_detail(call);
    }
}

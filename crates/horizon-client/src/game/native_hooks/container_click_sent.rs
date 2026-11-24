use super::*;
use horizon_protocol::{item::Hashed, packets::play::ContainerClick, registry::Builtin};
use mod_api::{ContainerClickSnapshot, PredictedClickStack};

impl Game<'_> {
    pub(in crate::game) fn observe_container_click(
        &self,
        click: &ContainerClick,
        builtin: &Builtin,
    ) {
        if !mod_api::__private::enabled_listened(mod_api::HookId::ContainerClickSent) {
            return;
        }
        let items = builtin.get("minecraft:item");
        let resolve = |stack: &Hashed| {
            PredictedClickStack::new(
                items
                    .and_then(|ids| ids.name(stack.item))
                    .map(str::to_owned),
                stack.count,
            )
        };
        self.native_container_click_sent(&ContainerClickSnapshot::new(
            click.container,
            click.state,
            click.slot,
            click.button,
            click.input,
            click
                .changed
                .iter()
                .map(|(slot, stack)| (*slot, stack.as_ref().map(resolve)))
                .collect(),
            click.carried.as_ref().map(resolve),
        ));
    }

    pub(in crate::game) fn native_container_click_sent(
        &self,
        click: &mod_api::ContainerClickSnapshot,
    ) {
        self.fire_container_click_sent(click);
    }
}

//! The `mob_effect_added_detail` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// A mob or player was given the effect `effect` (a qualified name, vanilla's and a content
    /// mod's alike), after the change (`LivingEntity.addEffect`; Bukkit's `EntityPotionEffectEvent`).
    /// `entity` is the mob's network id, none for the acting player. `duration` is in ticks, -1 for
    /// an infinite effect; `new` is true when the entity did not have the effect before; `cause` is
    /// `command` (`/effect` on a mob), `plugin` (a mod's own call) or `unknown`. Listen-only. A
    /// separate hook, because `mob_effect_added` only names content mods' effects and keeps its
    /// published signature (#19020). Server-side.
    #[hook(
        side = "server",
        contract = "listen",
        neoforge = "MobEffectEvent.Added",
        paper = "EntityPotionEffectEvent"
    )]
    fn mob_effect_added_detail(
        effect: &str,
        entity: Option<i32>,
        amplifier: i32,
        duration: i32,
        new: bool,
        cause: &str,
    );
}

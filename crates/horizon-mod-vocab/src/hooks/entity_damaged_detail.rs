//! The `entity_damaged_detail` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// An entity that `living_damage_post` never hears, today an armor stand, was hit by damage of
    /// `damage_type` (`minecraft:player_attack`, `minecraft:explosion`...) from the entity
    /// `causing` (its network id; the victim's own when nothing is named). `entity` is the victim's
    /// network id and `kind` its type (`minecraft:armor_stand`). Heard before the hit is applied,
    /// once for each hit a player's attack, a blast or burning makes on the stand, whether it only
    /// wobbles or breaks. Listen-only. A separate hook, because `living_damage_post` keeps its
    /// published signature and has no victim for a stand (#19020). Server-side.
    #[hook(
        side = "server",
        contract = "listen",
        neoforge = "none (an armor stand is a LivingEntity there; Horizon's is not)",
        paper = "EntityDamageEvent"
    )]
    fn entity_damaged_detail(entity: u32, kind: &str, damage_type: &str, causing: u32);
}

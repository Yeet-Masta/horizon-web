//! Who may hurt a mob: `LivingEntity.isInvulnerableTo`'s `Entity.isInvulnerableToBase` term,
//! and the dead-or-removed guard every damage hook runs after (`LivingIncomingDamageEvent`).
use super::{Damage, DamageKind, Mob, Who, gear};
use horizon_core::equipment_slot::EquipmentSlot;
use horizon_items::equipment::SLOT_NAMES;

impl Mob {
    /// A player returning a ghast's fireball gets past its fire immunity.
    pub(super) fn reflected_fireball(&self, source: &Damage) -> bool {
        &*self.kind.id == "minecraft:ghast"
            && source.kind == DamageKind::Fireball
            && source.attacker == Some(Who::Player)
    }

    /// `Entity.isInvulnerableToBase`'s invulnerability term: `Invulnerable` or
    /// `invulnerable_time` left, which only #bypasses_invulnerability or a blow a creative
    /// player caused (`DamageSource.isCreativePlayer`) gets past.
    pub fn is_invulnerable_to(&self, source: &Damage) -> bool {
        self.is_invulnerable() && !self.creative_attacker && !source.kind.bypasses_invulnerability()
    }
    /// `LivingEntity.isInvulnerableTo` and dead or removed: only #bypasses_invulnerability or a
    /// creative player's blow gets past the flag. The guard every damage hook runs after
    /// (`LivingIncomingDamageEvent`).
    pub fn refuses_damage(&self, source: &Damage) -> bool {
        self.extra.client_side
            || !self.alive()
            || self.removed
            || self.is_invulnerable_to(source)
            || (source.is_fire() && self.kind.flags.fire_immune && !self.reflected_fireball(source))
    }
}

/// `EquipmentSlotGroup.test`: whether an enchantment's slot group `group` covers `slot`.
/// `armor` is `EquipmentSlot.isArmor`: the humanoid pieces and an animal's body.
fn slot_group_covers(group: &str, slot: EquipmentSlot) -> bool {
    use EquipmentSlot as S;
    match group {
        "any" => true,
        "hand" => matches!(slot, S::MainHand | S::OffHand),
        "armor" => matches!(slot, S::Feet | S::Legs | S::Chest | S::Head | S::Body),
        _ => group == SLOT_NAMES[slot as usize],
    }
}

impl Mob {
    /// The enchantments on what the mob has in `slot` (`getItemBySlot(slot)`'s
    /// `minecraft:enchantments`): the hands' and armour's from `gear` over the kept stack,
    /// as `finalize::stored_stack` overlays them, the body's and saddle's from their stacks.
    fn slot_enchantment_levels(&self, slot: EquipmentSlot) -> &[(std::sync::Arc<str>, u32)] {
        let index = slot as usize;
        if index <= horizon_items::equipment::HEAD
            && let Some(levels) = gear::gear(self)
                .map(|g| &g.slots[super::finalize::gear_slot(index)])
                .filter(|levels| !levels.is_empty())
        {
            return levels;
        }
        self.extra
            .slot_stack(index)
            .map_or(&[], crate::items::ItemStack::enchantments)
    }

    /// `EnchantmentHelper.isImmuneToDamage`, the second term of `LivingEntity.isInvulnerableTo`:
    /// over `EquipmentSlot.VALUES`, each enchantment in a slot it works in
    /// (`Enchantment.matchingSlot`) asks `Enchantment.isImmuneToDamage`, whether one of its
    /// `minecraft:damage_immunity` effects' requirements hold for `source` (Frost Walker's
    /// `#burn_from_stepping` outside `#bypasses_invulnerability`: magma, a campfire). The first
    /// immune answer ends the asking, and with it the requirements' draws.
    pub fn immune_by_enchantment(
        &self,
        items: &crate::items::Items,
        source: &Damage,
        attack: &crate::health::Attack,
        random: &mut horizon_core::random::LegacyRandom,
    ) -> bool {
        let enchantments = &items.enchantments;
        let mut context = None;
        for slot in EquipmentSlot::ALL {
            for (id, level) in self.slot_enchantment_levels(slot) {
                let Some(def) = enchantments.get(id) else {
                    continue;
                };
                if !def.slots.iter().any(|group| slot_group_covers(group, slot)) {
                    continue;
                }
                let Some(effects) = def.effects["minecraft:damage_immunity"].as_array() else {
                    continue;
                };
                // `Enchantment.damageContext`: the victim, the source and its entities.
                let ctx = context.get_or_insert_with(|| {
                    let hurt = crate::health::mob_source(source, attack);
                    let attacker = match source.attacker {
                        Some(Who::Player) => Some("minecraft:player"),
                        Some(Who::Mob(_)) => {
                            source.attacker_species.map(super::species::vanilla_id)
                        }
                        None => None,
                    };
                    let is_direct = hurt.causing == hurt.direct;
                    crate::enchant::EffectContext {
                        items: Some(items),
                        damage_type: Some(hurt.damage_type),
                        is_direct,
                        this: Some(&self.kind.id),
                        attacker,
                        // A projectile's own type is not known here; the attacker is the
                        // direct one of a melee blow.
                        direct_attacker: attacker.filter(|_| is_direct),
                        ..Default::default()
                    }
                });
                if effects.iter().any(|effect| {
                    effect
                        .get("requirements")
                        .is_none_or(|r| enchantments.condition(r, *level, ctx, random))
                }) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Vanilla's `frost_walker.json`, its `damage_immunity` effect and slots (the frosted-ice
    /// `location_changed` effect left out), and the two damage tags it reads.
    fn frost_walker_items() -> crate::items::Items {
        let frost_walker = serde_json::json!({
            "anvil_cost": 4,
            "description": {"translate": "enchantment.minecraft.frost_walker"},
            "effects": {"minecraft:damage_immunity": [{"effect": {}, "requirements": {
                "type": "minecraft:damage_source_properties",
                "predicate": {"tags": [
                    {"expected": true, "id": "#minecraft:burn_from_stepping"},
                    {"expected": false, "id": "#minecraft:bypasses_invulnerability"}
                ]}
            }}]},
            "max_cost": {"base": 25, "per_level_above_first": 10},
            "max_level": 2,
            "min_cost": {"base": 10, "per_level_above_first": 10},
            "slots": ["feet"],
            "supported_items": "#minecraft:enchantable/foot_armor",
            "weight": 2
        });
        let mut enchantments = crate::enchant::Enchantments::default()
            .with_entries([("minecraft:frost_walker", &frost_walker)])
            .unwrap()
            .unwrap();
        for (tag, types) in [
            (
                "minecraft:burn_from_stepping",
                &[
                    "minecraft:campfire",
                    "minecraft:hot_floor",
                    "minecraft:sulfur_cube_hot",
                ][..],
            ),
            (
                "minecraft:bypasses_invulnerability",
                &["minecraft:out_of_world", "minecraft:generic_kill"][..],
            ),
        ] {
            enchantments.damage_tags.insert(
                tag.to_owned(),
                types.iter().map(|t| (*t).to_owned()).collect(),
            );
        }
        let mut items = crate::items::Items::empty();
        items.enchantments = Arc::new(enchantments);
        items
    }

    fn zombie_with_frost_walker(slot: usize) -> Mob {
        let mut kind = crate::mobs::sensor_scan_tests::kind();
        let k = Arc::make_mut(&mut kind);
        k.id = "minecraft:zombie".into();
        k.species = crate::mobs::Species::Zombie;
        let mut m = Mob::new(1, kind, horizon_core::geometry::Vec3::ZERO, 0.0);
        let mut boots = crate::items::ItemStack::new("minecraft:iron_boots", 1);
        boots.data_mut().enchantments = vec![(Arc::from("minecraft:frost_walker"), 1)];
        crate::mobs::finalize::set_item_stack(&mut m, slot, boots);
        m
    }

    /// `LivingEntity.isInvulnerableTo`'s `EnchantmentHelper.isImmuneToDamage`: Frost Walker
    /// boots on the feet refuse magma and campfire damage, not other damage, and boots held
    /// in the hand (a slot Frost Walker doesn't work in) refuse nothing.
    #[test]
    fn frost_walker_on_the_feet_refuses_burn_from_stepping_damage() {
        use crate::mobs::finalize::{FEET, MAINHAND};
        let items = frost_walker_items();
        let mut level = crate::health::TestLevel::new(0);
        for kind in [DamageKind::HotFloor, DamageKind::Campfire] {
            let mut worn = zombie_with_frost_walker(FEET);
            let health = worn.health;
            assert!(
                !worn.damage(&mut level.level_with(&items), 1.0, Damage::of(kind)),
                "{kind:?} hurt the zombie"
            );
            assert_eq!(worn.health, health);
            let mut held = zombie_with_frost_walker(MAINHAND);
            assert!(held.damage(&mut level.level_with(&items), 1.0, Damage::of(kind)));
            assert!(held.health < health);
        }
        let mut worn = zombie_with_frost_walker(FEET);
        assert!(worn.damage(
            &mut level.level_with(&items),
            1.0,
            Damage::of(DamageKind::Cactus)
        ));
    }
}

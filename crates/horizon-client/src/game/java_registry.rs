//! Typed declarations collected while Java entrypoints bootstrap.
//!
//! The snapshot is the bounded, deterministic view of every declared row the
//! run's Java mods produced. It is built beside the content root, from the same
//! [`NativeDeclarations`], so diagnostics, the compatibility summary and the
//! content model never disagree. Rows the content model cannot express yet
//! (block entities, recipes, ingredients) are kept here so they stay visible
//! instead of silently disappearing.
#![allow(dead_code)]
use horizon_mods::content::{Block, EntityType, Item, MobEffect};
use horizon_mods::native::{
    BlockEntityDeclaration, MAX_NATIVE_DECLARATIONS, MenuDeclaration, NativeDeclarations,
};
use std::{
    collections::BTreeMap,
    sync::{OnceLock, RwLock},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct JavaRegistrySnapshot {
    pub(super) blocks: Vec<String>,
    pub(super) items: Vec<String>,
    pub(super) effects: Vec<String>,
    pub(super) entities: Vec<String>,
    pub(super) menus: Vec<String>,
    pub(super) block_entities: Vec<String>,
    pub(super) block_templates: Vec<Block>,
    pub(super) item_templates: Vec<Item>,
    pub(super) effect_templates: Vec<MobEffect>,
    pub(super) entity_templates: Vec<EntityType>,
    pub(super) menu_templates: Vec<MenuDeclaration>,
    pub(super) block_entity_templates: Vec<BlockEntityDeclaration>,
    /// Rows folded away while coalescing duplicates, by table name. Sorted, so
    /// two batches that coalesce the same rows report the same map.
    pub(super) coalesced: BTreeMap<String, usize>,
}

impl JavaRegistrySnapshot {
    /// Builds the snapshot from the declarations the run's jars recorded.
    ///
    /// Rows are sorted by id, so metadata order and entrypoint execution order
    /// cannot change the snapshot. A row repeated by a later jar coalesces
    /// when it is equal and is counted in [`Self::coalesced`]; two rows of a
    /// table that disagree on any field are an error naming that id instead of
    /// a silent first-wins. The whole set is bounded by the same row cap the
    /// declaration sink uses.
    pub(super) fn from_declarations(sets: &[NativeDeclarations]) -> Result<Self, String> {
        let mut next = Self::default();
        for declarations in sets {
            next.blocks
                .extend(declarations.blocks.iter().map(|row| row.id.clone()));
            next.items
                .extend(declarations.items.iter().map(|row| row.id.clone()));
            next.effects
                .extend(declarations.effects.iter().map(|row| row.id.clone()));
            next.entities
                .extend(declarations.entities.iter().map(|row| row.id.clone()));
            next.menus
                .extend(declarations.menus.iter().map(|row| row.id.clone()));
            next.block_entities
                .extend(declarations.block_entities.iter().map(|row| row.id.clone()));
            next.block_templates
                .extend(declarations.blocks.iter().cloned());
            next.item_templates
                .extend(declarations.items.iter().cloned());
            next.effect_templates
                .extend(declarations.effects.iter().cloned());
            next.entity_templates
                .extend(declarations.entities.iter().cloned());
            next.menu_templates
                .extend(declarations.menus.iter().cloned());
            next.block_entity_templates
                .extend(declarations.block_entities.iter().cloned());
        }
        let rows = next.blocks.len()
            + next.items.len()
            + next.effects.len()
            + next.entities.len()
            + next.menus.len()
            + next.block_entities.len();
        if rows > MAX_NATIVE_DECLARATIONS {
            return Err(format!(
                "java registry has {rows} rows, over the {MAX_NATIVE_DECLARATIONS} declaration bound"
            ));
        }
        for ids in [
            &mut next.blocks,
            &mut next.items,
            &mut next.effects,
            &mut next.entities,
            &mut next.menus,
            &mut next.block_entities,
        ] {
            ids.sort();
            ids.dedup();
        }
        let mut coalesced = BTreeMap::new();
        note_coalesced(
            &mut coalesced,
            "blocks",
            coalesce_typed(&mut next.block_templates, "block", |row| row.id.as_str())?,
        );
        note_coalesced(
            &mut coalesced,
            "items",
            coalesce_typed(&mut next.item_templates, "item", |row| row.id.as_str())?,
        );
        note_coalesced(
            &mut coalesced,
            "mob effects",
            coalesce_typed(&mut next.effect_templates, "mob effect", |row| {
                row.id.as_str()
            })?,
        );
        note_coalesced(
            &mut coalesced,
            "entities",
            coalesce_typed(&mut next.entity_templates, "entity", |row| row.id.as_str())?,
        );
        note_coalesced(
            &mut coalesced,
            "menus",
            coalesce_typed(&mut next.menu_templates, "menu", |row| row.id.as_str())?,
        );
        note_coalesced(
            &mut coalesced,
            "block entities",
            coalesce_typed(&mut next.block_entity_templates, "block entity", |row| {
                row.id.as_str()
            })?,
        );
        next.coalesced = coalesced;
        Ok(next)
    }

    /// Counts per table, in a fixed order, for the compatibility summary.
    pub(super) fn counts(&self) -> [(&'static str, usize); 6] {
        [
            ("blocks", self.blocks.len()),
            ("items", self.items.len()),
            ("mob effects", self.effects.len()),
            ("entities", self.entities.len()),
            ("menus", self.menus.len()),
            ("block entities", self.block_entities.len()),
        ]
    }

    /// Deterministic one-line summary; a table's ids and its templates are
    /// counted separately to show what a lookup sees and what was retained.
    pub(super) fn diagnostics(&self) -> Vec<String> {
        let counts: Vec<String> = self
            .counts()
            .iter()
            .map(|(kind, count)| format!("{count} {kind}"))
            .collect();
        let mut out = vec![format!("java registry: {}", counts.join(", "))];
        if !self.coalesced.is_empty() {
            let rows = self
                .coalesced
                .iter()
                .map(|(table, count)| format!("{table}={count}"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!("java registry coalesced duplicate rows: {rows}"));
        }
        out
    }
}

static SNAPSHOT: OnceLock<RwLock<JavaRegistrySnapshot>> = OnceLock::new();

fn state() -> &'static RwLock<JavaRegistrySnapshot> {
    SNAPSHOT.get_or_init(|| RwLock::new(JavaRegistrySnapshot::default()))
}

/// Publishes the run's declarations, or keeps the previous snapshot when a
/// row is invalid. Publishing is all-or-nothing, so a rejected batch never
/// leaves half a table visible.
pub(super) fn publish(declarations: &[NativeDeclarations]) -> Result<(), String> {
    let next = JavaRegistrySnapshot::from_declarations(declarations)?;
    *state().write().expect("java registry lock poisoned") = next;
    Ok(())
}

/// The snapshot the last published batch produced.
pub(super) fn snapshot() -> JavaRegistrySnapshot {
    state().read().expect("java registry lock poisoned").clone()
}

/// Typed rows are validated before coalescing: only a row that agrees on every
/// field may share another row's id. `table` names the row kind in the error.
fn coalesce_typed<T: PartialEq>(
    rows: &mut Vec<T>,
    table: &str,
    id: impl Fn(&T) -> &str,
) -> Result<usize, String> {
    rows.sort_by(|a, b| id(a).cmp(id(b)));
    if let Some(pair) = rows
        .windows(2)
        .find(|pair| id(&pair[0]) == id(&pair[1]) && pair[0] != pair[1])
    {
        return Err(format!(
            "conflicting native {table} declaration {}",
            id(&pair[0])
        ));
    }
    let before = rows.len();
    rows.dedup_by(|a, b| id(a) == id(b));
    Ok(before - rows.len())
}

fn note_coalesced(coalesced: &mut BTreeMap<String, usize>, table: &str, folded: usize) {
    if folded > 0 {
        *coalesced.entry(table.to_string()).or_default() += folded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: &str, template: &str) -> Block {
        Block {
            id: id.into(),
            template: template.into(),
            item: true,
            ..Block::default()
        }
    }

    #[test]
    fn snapshot_is_sorted_and_coalesces_equal_rows() {
        let first = NativeDeclarations {
            blocks: vec![
                block("ruby:z", "minecraft:stone"),
                block("ruby:a", "minecraft:stone"),
            ],
            ..NativeDeclarations::default()
        };
        let second = NativeDeclarations {
            blocks: vec![block("ruby:a", "minecraft:stone")],
            ..NativeDeclarations::default()
        };
        let snapshot = JavaRegistrySnapshot::from_declarations(&[first, second]).unwrap();
        assert_eq!(snapshot.blocks, ["ruby:a", "ruby:z"]);
        assert_eq!(snapshot.block_templates.len(), 2);
        assert_eq!(snapshot.coalesced.get("blocks"), Some(&1));
    }

    #[test]
    fn a_conflicting_row_is_refused() {
        let first = NativeDeclarations {
            blocks: vec![block("ruby:a", "minecraft:stone")],
            ..NativeDeclarations::default()
        };
        let second = NativeDeclarations {
            blocks: vec![block("ruby:a", "minecraft:dirt")],
            ..NativeDeclarations::default()
        };
        let error = JavaRegistrySnapshot::from_declarations(&[first, second]).unwrap_err();
        assert!(
            error.contains("conflicting native block declaration ruby:a"),
            "{error}"
        );
    }
}

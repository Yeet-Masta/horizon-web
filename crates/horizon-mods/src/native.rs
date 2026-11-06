//! Typed declarations exported by the native Java compatibility boundary.
//!
//! Java registration callbacks do not carry enough information to construct a
//! Horizon block or item by themselves. A loader adapter first enriches a
//! `horizon_native_java::RegistryEntry` with a typed declaration, then this
//! sink validates the small declaration vocabulary used by the content loader
//! and turns it into the current [`crate::content`] model: blocks, items, mob
//! effects, entity types and menu types. Block entities, recipes, ingredients
//! and custom registrations are retained for diagnostics, since the content
//! model has no table for them yet.
//!
//! The module deliberately names no `horizon-native-java` type: the loader
//! adapter in the game matches on the Java declaration and calls the `add_*`
//! methods here, so this crate keeps depending on nothing but its content
//! model.

use crate::content::{
    Block, BlockProperties, Content, ENTITY_TEMPLATES, EffectCategory, EntityType, Item, MenuType,
    MobEffect, Rarity, RenderShape,
};
use std::collections::{BTreeMap, BTreeSet};

/// Native declarations are metadata, not an unbounded Java object graph.
pub const MAX_NATIVE_DECLARATIONS: usize = 8_192;
pub const MAX_NATIVE_PROPERTIES: usize = 32;
pub const MAX_NATIVE_TEXT: usize = 4_096;
/// Recipe and ingredient JSON is data, not executable code; keep each declaration bounded.
pub const MAX_NATIVE_JSON: usize = 1 << 20;
/// Vanilla attribute defaults are plain doubles; this is the largest one a
/// declaration may set (health, damage, follow range).
pub const MAX_ENTITY_ATTRIBUTE: f64 = 1.0e6;
/// The blocks that open one declared menu (`open_blocks`). A station is opened
/// by a handful of blocks; 64 keeps the row bounded and matches
/// [`MAX_BLOCK_ENTITY_BLOCKS`].
pub const MAX_MENU_OPEN_BLOCKS: usize = 64;
/// `BlockEntityType.validBlocks` for one declared type. Vanilla's longest
/// list (the signs') stays far below this.
pub const MAX_BLOCK_ENTITY_BLOCKS: usize = 64;

/// A custom registration kind whose concrete Horizon adapter is not available
/// yet. It is retained so diagnostics can name the registration precisely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomDeclaration {
    pub registry: String,
    pub id: String,
    pub kind: String,
    pub properties: BTreeMap<String, String>,
}

/// A recipe declaration that would become a data-pack JSON file. The current
/// content model has no code-declared data files, so it is retained only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeDeclaration {
    pub id: String,
    pub json: Vec<u8>,
}

/// An ingredient declaration, retained beside [`RecipeDeclaration`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngredientDeclaration {
    pub id: String,
    pub json: Vec<u8>,
}

/// A typed menu registration, as far as the content model can express it: its
/// slot layout. `title_key` and `open_blocks` come from the older declaration
/// vocabulary and are validated and retained for the registry snapshot; the
/// current [`MenuType`] has no fields for them yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuDeclaration {
    pub id: String,
    pub slots: u16,
    pub data_slots: u16,
    pub results: Vec<u16>,
    pub title_key: Option<String>,
    pub open_blocks: Vec<String>,
}

impl MenuDeclaration {
    /// Parses one native menu row from the free-form keys a loader adapter
    /// records. `slots` is required; every other key is optional. Unknown
    /// keys are rejected so a typo cannot silently drop a constructor option.
    pub fn from_registry_properties(
        id: impl Into<String>,
        properties: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut slots = None;
        let mut data_slots = 0;
        let mut results = Vec::new();
        let mut title_key = None;
        let mut open_blocks = Vec::new();
        for (key, value) in properties {
            match key.as_str() {
                "slots" => slots = Some(parse_u16(key, value)?),
                "data_slots" => data_slots = parse_u16(key, value)?,
                "results" => results = parse_u16_list(key, value)?,
                "title_key" => {
                    if value.is_empty()
                        || value.len() > MAX_NATIVE_TEXT
                        || !value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                    {
                        return Err(format!(
                            "native menu property title_key {value:?} is a key of a-z0-9_.-"
                        ));
                    }
                    title_key = Some(value.clone());
                }
                "open_blocks" => open_blocks = parse_id_list(key, value, MAX_MENU_OPEN_BLOCKS)?,
                other => return Err(format!("unknown native menu property {other:?}")),
            }
        }
        let menu = Self {
            id: id.into(),
            slots: slots.ok_or_else(|| "native menu has no slots".to_owned())?,
            data_slots,
            results,
            title_key,
            open_blocks,
        };
        menu.validate()?;
        Ok(menu)
    }

    /// The fences a menu row passes whether the sink parsed it or it was built
    /// by hand.
    pub fn validate(&self) -> Result<(), String> {
        NativeDeclarations::ensure_id(&self.id)?;
        if !(1..=crate::content::MAX_MENU_SLOTS).contains(&self.slots) {
            return Err(format!(
                "native menu {} slots is 1 to {}",
                self.id,
                crate::content::MAX_MENU_SLOTS
            ));
        }
        if self.data_slots > crate::content::MAX_MENU_SLOTS {
            return Err(format!(
                "native menu {} data_slots is 0 to {}",
                self.id,
                crate::content::MAX_MENU_SLOTS
            ));
        }
        let mut seen = BTreeSet::new();
        for &result in &self.results {
            if result >= self.slots {
                return Err(format!(
                    "native menu {} result {result} is past the last slot, {}",
                    self.id,
                    self.slots - 1
                ));
            }
            if !seen.insert(result) {
                return Err(format!(
                    "native menu {} result {result} is listed twice",
                    self.id
                ));
            }
        }
        if self.open_blocks.len() > MAX_MENU_OPEN_BLOCKS {
            return Err(format!(
                "native menu {} has at most {MAX_MENU_OPEN_BLOCKS} open blocks",
                self.id
            ));
        }
        for block in &self.open_blocks {
            NativeDeclarations::ensure_id(block)?;
        }
        Ok(())
    }

    /// The row the content loader reads.
    pub fn menu_type(&self) -> MenuType {
        MenuType {
            id: self.id.clone(),
            slots: self.slots,
            data_slots: self.data_slots,
            results: self.results.clone(),
        }
    }
}

/// A typed block-entity registration: a vanilla `BlockEntityType` template
/// under a new id, the blocks that carry it and whether it ticks. The content
/// model has no block-entity table yet, so this row stays a declaration the
/// registry snapshot exposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockEntityDeclaration {
    pub id: String,
    /// The vanilla block entity type it copies, `minecraft:<path>`.
    pub template: String,
    /// Whether the game ticks it (`BlockEntityTicker`).
    pub ticker: bool,
    /// `BlockEntityType.validBlocks`: the blocks that carry it, sorted.
    pub valid_blocks: Vec<String>,
}

impl BlockEntityDeclaration {
    /// Parses one native block-entity row. `template` (a vanilla block entity
    /// type) and `valid_blocks` (comma-separated ids) are required; `ticker`
    /// defaults to false. Every other key is rejected.
    pub fn from_registry_properties(
        id: impl Into<String>,
        properties: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut template = None;
        let mut ticker = false;
        let mut valid_blocks = None;
        for (key, value) in properties {
            match key.as_str() {
                "template" => template = Some(value.clone()),
                "ticker" => ticker = parse_bool(key, value)?,
                "valid_blocks" => {
                    valid_blocks = Some(parse_id_list(key, value, MAX_BLOCK_ENTITY_BLOCKS)?)
                }
                other => return Err(format!("unknown native block entity property {other:?}")),
            }
        }
        let block_entity = Self {
            id: id.into(),
            template: template.ok_or_else(|| "native block entity has no template".to_owned())?,
            ticker,
            valid_blocks: valid_blocks
                .ok_or_else(|| "native block entity has no valid_blocks".to_owned())?,
        };
        block_entity.validate()?;
        Ok(block_entity)
    }

    /// The fences a block-entity row passes whether the sink parsed it or it
    /// was built by hand.
    pub fn validate(&self) -> Result<(), String> {
        NativeDeclarations::ensure_id(&self.id)?;
        NativeDeclarations::ensure_id(&self.template)?;
        if !self.template.starts_with("minecraft:") {
            return Err(format!(
                "native block entity {} template {:?} must be a vanilla id",
                self.id, self.template
            ));
        }
        if self.valid_blocks.is_empty() || self.valid_blocks.len() > MAX_BLOCK_ENTITY_BLOCKS {
            return Err(format!(
                "native block entity {} has 1 to {MAX_BLOCK_ENTITY_BLOCKS} valid blocks",
                self.id
            ));
        }
        let mut sorted = self.valid_blocks.clone();
        sorted.sort();
        if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(format!(
                "native block entity {} valid_blocks repeats an id",
                self.id
            ));
        }
        for block in &self.valid_blocks {
            NativeDeclarations::ensure_id(block)?;
        }
        Ok(())
    }
}

/// Typed registrations waiting for the pre-resource load phase.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct NativeDeclarations {
    pub blocks: Vec<Block>,
    pub items: Vec<Item>,
    pub effects: Vec<MobEffect>,
    pub entities: Vec<EntityType>,
    pub menus: Vec<MenuDeclaration>,
    pub block_entities: Vec<BlockEntityDeclaration>,
    pub recipes: Vec<RecipeDeclaration>,
    pub ingredients: Vec<IngredientDeclaration>,
    pub custom: Vec<CustomDeclaration>,
}

impl NativeDeclarations {
    /// Merge declarations from another adapter transactionally.
    ///
    /// Equal rows are coalesced, while a row with the same id and different
    /// values is rejected. Rows are sorted by id after the merge so metadata
    /// order and entrypoint execution order cannot change the resource plan.
    pub fn merge(&mut self, incoming: Self) -> Result<(), String> {
        let mut staged = self.clone();
        merge_rows("block", &mut staged.blocks, incoming.blocks, |row| &row.id)?;
        merge_rows("item", &mut staged.items, incoming.items, |row| &row.id)?;
        merge_rows("mob effect", &mut staged.effects, incoming.effects, |row| {
            &row.id
        })?;
        merge_rows("entity", &mut staged.entities, incoming.entities, |row| {
            &row.id
        })?;
        merge_rows("menu", &mut staged.menus, incoming.menus, |row| &row.id)?;
        merge_rows(
            "block entity",
            &mut staged.block_entities,
            incoming.block_entities,
            |row| &row.id,
        )?;
        merge_rows("recipe", &mut staged.recipes, incoming.recipes, |row| {
            &row.id
        })?;
        merge_rows(
            "ingredient",
            &mut staged.ingredients,
            incoming.ingredients,
            |row| &row.id,
        )?;
        for row in incoming.custom {
            if let Some(existing) = staged
                .custom
                .iter()
                .find(|existing| existing.registry == row.registry && existing.id == row.id)
            {
                if existing != &row {
                    return Err(format!(
                        "conflicting native custom registration {}/{}",
                        row.registry, row.id
                    ));
                }
            } else if staged
                .custom
                .iter()
                .any(|existing| existing.id == row.id && existing.registry != row.registry)
            {
                return Err(format!("conflicting native custom registration {}", row.id));
            } else {
                staged.custom.push(row);
            }
        }
        staged.custom.sort_by(|a, b| {
            a.registry
                .cmp(&b.registry)
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        staged.check_count()?;
        *self = staged;
        Ok(())
    }

    fn check_count(&self) -> Result<(), String> {
        let count = self.blocks.len()
            + self.items.len()
            + self.effects.len()
            + self.entities.len()
            + self.menus.len()
            + self.block_entities.len()
            + self.recipes.len()
            + self.ingredients.len()
            + self.custom.len();
        if count > MAX_NATIVE_DECLARATIONS {
            return Err(format!(
                "native declarations exceed {MAX_NATIVE_DECLARATIONS} rows"
            ));
        }
        Ok(())
    }

    /// A short summary of the declaration set, for the launcher's mod page.
    /// Purely informational: it validates nothing and changes nothing.
    pub fn diagnostics(&self) -> Vec<String> {
        vec![format!(
            "native declarations: {} blocks, {} items, {} mob effects, {} entities, {} menus, {} block entities, {} recipes, {} ingredients, {} custom",
            self.blocks.len(),
            self.items.len(),
            self.effects.len(),
            self.entities.len(),
            self.menus.len(),
            self.block_entities.len(),
            self.recipes.len(),
            self.ingredients.len(),
            self.custom.len(),
        )]
    }

    fn ensure_id(id: &str) -> Result<(), String> {
        let Some((namespace, path)) = id.split_once(':') else {
            return Err(format!("native registration {id:?} has no namespace"));
        };
        if id.len() > MAX_NATIVE_TEXT
            || namespace.is_empty()
            || path.is_empty()
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
            || !path
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_./-".contains(&b))
        {
            return Err(format!("native registration id {id:?} is invalid"));
        }
        Ok(())
    }

    fn ensure_registration_id(id: &str) -> Result<(), String> {
        Self::ensure_id(id)?;
        if id.starts_with("minecraft:") {
            return Err(format!(
                "native registration {id:?} cannot redefine the minecraft namespace"
            ));
        }
        Ok(())
    }

    fn check_properties(kind: &str, properties: &BTreeMap<String, String>) -> Result<(), String> {
        if properties.len() > MAX_NATIVE_PROPERTIES {
            return Err(format!(
                "native {kind} has more than {MAX_NATIVE_PROPERTIES} properties"
            ));
        }
        if properties.iter().any(|(key, value)| {
            key.is_empty() || key.len() > MAX_NATIVE_TEXT || value.len() > MAX_NATIVE_TEXT
        }) {
            return Err(format!(
                "native {kind} property names and values are at most {MAX_NATIVE_TEXT} bytes"
            ));
        }
        Ok(())
    }

    fn parse_block_properties(
        properties: &BTreeMap<String, String>,
    ) -> Result<BlockProperties, String> {
        let mut out = BlockProperties::default();
        for (key, value) in properties {
            match key.as_str() {
                "hardness" => out.hardness = Some(parse_f32(key, value)?),
                "explosion_resistance" => out.explosion_resistance = Some(parse_f32(key, value)?),
                "light" => out.light = Some(parse_u8(key, value)?),
                "requires_tool" => out.requires_tool = Some(parse_bool(key, value)?),
                "friction" => out.friction = Some(parse_f32(key, value)?),
                "map_color" => out.map_color = Some(parse_u8(key, value)?),
                "sounds" => out.sounds = Some(value.clone()),
                "occludes" => out.occludes = Some(parse_bool(key, value)?),
                "render_shape" => {
                    out.render_shape = Some(match value.as_str() {
                        "MODEL" => RenderShape::Model,
                        "ENTITYBLOCK_ANIMATED" => RenderShape::EntityblockAnimated,
                        "INVISIBLE" => RenderShape::Invisible,
                        _ => {
                            return Err(format!(
                                "native block property render_shape {value:?} is MODEL, ENTITYBLOCK_ANIMATED or INVISIBLE"
                            ));
                        }
                    })
                }
                other => return Err(format!("unknown native block property {other:?}")),
            }
        }
        Ok(out)
    }

    /// Adds one typed block registration.
    pub fn add_block(
        &mut self,
        id: &str,
        template: &str,
        properties: &BTreeMap<String, String>,
        item: bool,
    ) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        Self::check_properties("block", properties)?;
        Self::ensure_id(template)?;
        if self.blocks.iter().any(|block| block.id == id) {
            return Err(format!("duplicate native block {id}"));
        }
        if item && self.items.iter().any(|existing| existing.id == id) {
            return Err(format!("native block {id} conflicts with an item"));
        }
        self.blocks.push(Block {
            id: id.to_owned(),
            template: template.to_owned(),
            properties: Self::parse_block_properties(properties)?,
            item,
            ..Block::default()
        });
        self.check_count()
    }

    /// Adds one typed item registration.
    pub fn add_item(
        &mut self,
        id: &str,
        template: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        Self::check_properties("item", properties)?;
        Self::ensure_id(template)?;
        if self.items.iter().any(|item| item.id == id) {
            return Err(format!("duplicate native item {id}"));
        }
        if self.blocks.iter().any(|block| block.id == id && block.item) {
            return Err(format!("native item {id} conflicts with a block item"));
        }
        let mut max_stack_size = None;
        let mut rarity = None;
        let mut crafting_remainder = None;
        let mut components = BTreeMap::new();
        for (key, value) in properties {
            match key.as_str() {
                "max_stack_size" => max_stack_size = Some(parse_u8(key, value)?),
                "rarity" => {
                    rarity = Some(match value.to_ascii_lowercase().as_str() {
                        "common" => Rarity::Common,
                        "uncommon" => Rarity::Uncommon,
                        "rare" => Rarity::Rare,
                        "epic" => Rarity::Epic,
                        _ => return Err(format!("invalid native item rarity {value:?}")),
                    })
                }
                "crafting_remainder" => {
                    Self::ensure_id(value)?;
                    crafting_remainder = Some(value.clone());
                }
                key if key.starts_with("component:") => {
                    components.insert(
                        key.trim_start_matches("component:").to_owned(),
                        serde_json::Value::String(value.clone()),
                    );
                }
                other => return Err(format!("unknown native item property {other:?}")),
            }
        }
        self.items.push(Item {
            id: id.to_owned(),
            template: template.to_owned(),
            max_stack_size,
            rarity,
            components,
            crafting_remainder,
        });
        self.check_count()
    }

    /// Adds one typed mob effect registration.
    pub fn add_effect(&mut self, id: &str, category: &str, color: u32) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        if color > 0xFF_FFFF {
            return Err(format!(
                "native mob effect {id} color is RGB, 0 to 16777215 (0xFFFFFF)"
            ));
        }
        if self.effects.iter().any(|effect| effect.id == id) {
            return Err(format!("duplicate native mob effect {id}"));
        }
        let category = match category.to_ascii_lowercase().as_str() {
            "beneficial" => EffectCategory::Beneficial,
            "harmful" => EffectCategory::Harmful,
            "neutral" => EffectCategory::Neutral,
            _ => return Err(format!("invalid native mob effect category {category:?}")),
        };
        self.effects.push(MobEffect {
            id: id.to_owned(),
            category,
            color,
        });
        self.check_count()
    }

    /// Adds one typed entity type from the free-form keys a loader adapter
    /// records. `template` names one of [`ENTITY_TEMPLATES`]; a thrown type
    /// also names the item that throws it and its damage, a living one its
    /// default attributes (`max_health`, `speed`, `attack_damage`,
    /// `follow_range`). Keys the current content model cannot express are
    /// rejected rather than silently dropped.
    pub fn add_entity(
        &mut self,
        id: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        Self::check_properties("entity", properties)?;
        if self.entities.iter().any(|entity| entity.id == id) {
            return Err(format!("duplicate native entity {id}"));
        }
        let template = properties
            .get("template")
            .ok_or_else(|| format!("native entity {id} has no template"))?;
        if !ENTITY_TEMPLATES.contains(&template.as_str()) {
            return Err(format!(
                "native entity {id} template {template:?} is not one of {ENTITY_TEMPLATES:?}"
            ));
        }
        let living = crate::content::LIVING_TEMPLATES.contains(&template.as_str());
        let mut item = None;
        let mut damage = 0.0f32;
        let mut attributes = BTreeMap::new();
        for (key, value) in properties {
            match key.as_str() {
                "template" => {}
                "item" => {
                    Self::ensure_id(value)?;
                    item = Some(value.clone());
                }
                "damage" => {
                    damage = parse_f32(key, value)?;
                    if damage < 0.0 {
                        return Err("native entity property damage is 0 or more".into());
                    }
                }
                "max_health" => {
                    attributes.insert(
                        "minecraft:max_health".to_owned(),
                        parse_attribute(key, value)?,
                    );
                }
                "speed" => {
                    attributes.insert(
                        "minecraft:movement_speed".to_owned(),
                        parse_attribute(key, value)?,
                    );
                }
                "attack_damage" => {
                    attributes.insert(
                        "minecraft:attack_damage".to_owned(),
                        parse_attribute(key, value)?,
                    );
                }
                "follow_range" => {
                    attributes.insert(
                        "minecraft:follow_range".to_owned(),
                        parse_attribute(key, value)?,
                    );
                }
                other => return Err(format!("unknown native entity property {other:?}")),
            }
        }
        if living {
            if item.is_some() || damage != 0.0 {
                return Err(format!(
                    "native entity {id}: a living type has no item or damage (its template is {template})"
                ));
            }
        } else {
            if !attributes.is_empty() {
                return Err(format!(
                    "native entity {id}: only a living type has attributes (its template is {template})"
                ));
            }
            if item.is_none() {
                return Err(format!(
                    "native entity {id}: a thrown type names the item that throws it"
                ));
            }
        }
        self.entities.push(EntityType {
            id: id.to_owned(),
            template: template.clone(),
            item,
            damage,
            attributes,
        });
        self.check_count()
    }

    /// Adds one typed menu registration.
    pub fn add_menu(
        &mut self,
        id: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        Self::check_properties("menu", properties)?;
        if self.menus.iter().any(|menu| menu.id == id) {
            return Err(format!("duplicate native menu {id}"));
        }
        self.menus
            .push(MenuDeclaration::from_registry_properties(id, properties)?);
        self.check_count()
    }

    /// Adds one typed block-entity registration.
    pub fn add_block_entity(
        &mut self,
        id: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        Self::check_properties("block entity", properties)?;
        if self.block_entities.iter().any(|row| row.id == id) {
            return Err(format!("duplicate native block entity {id}"));
        }
        self.block_entities
            .push(BlockEntityDeclaration::from_registry_properties(
                id, properties,
            )?);
        self.check_count()
    }

    /// Adds one bounded recipe declaration.
    pub fn add_recipe(&mut self, id: &str, json: &str) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        if self.recipes.iter().any(|recipe| recipe.id == id) {
            return Err(format!("duplicate native recipe {id}"));
        }
        self.recipes.push(RecipeDeclaration {
            id: id.to_owned(),
            json: bounded_json("recipe", id, json)?,
        });
        self.check_count()
    }

    /// Adds one bounded ingredient declaration.
    pub fn add_ingredient(&mut self, id: &str, json: &str) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        if self.ingredients.iter().any(|row| row.id == id) {
            return Err(format!("duplicate native ingredient {id}"));
        }
        self.ingredients.push(IngredientDeclaration {
            id: id.to_owned(),
            json: bounded_json("ingredient", id, json)?,
        });
        self.check_count()
    }

    /// Adds one registration kind this build has no adapter for, so
    /// diagnostics can name it.
    pub fn add_custom(
        &mut self,
        registry: &str,
        id: &str,
        kind: &str,
        properties: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        Self::ensure_registration_id(id)?;
        Self::check_properties("custom", properties)?;
        if self.custom.iter().any(|row| row.id == id) {
            return Err(format!("duplicate native custom registration {id}"));
        }
        self.custom.push(CustomDeclaration {
            registry: registry.to_owned(),
            id: id.to_owned(),
            kind: kind.to_owned(),
            properties: properties.clone(),
        });
        self.check_count()
    }

    /// The declarations as content, one `(namespace, [`Content`])` per
    /// namespace they use, sorted by namespace. Each group is checked with the
    /// same fences a content file is ([`Content::declare`]); effects, entity
    /// types and menu types are checked here because the content model has no
    /// declaration files for them.
    ///
    /// Rows the content model cannot hold are left out of the content and
    /// stay available on the declaration set: block entities, recipes,
    /// ingredients, and the menus' title key and open blocks. Any custom
    /// registration is an error, as it was before the port: a Java mod that
    /// registers something Horizon has no adapter for must not half-load.
    pub fn content_mods(&self) -> Result<Vec<(String, Content)>, String> {
        if !self.custom.is_empty() {
            let mut details = self
                .custom
                .iter()
                .take(8)
                .map(|custom| format!("{}/{} ({})", custom.registry, custom.id, custom.kind))
                .collect::<Vec<_>>();
            if self.custom.len() > details.len() {
                details.push("...".to_owned());
            }
            return Err(format!(
                "{} unsupported custom registration(s): {}",
                self.custom.len(),
                details.join(", ")
            ));
        }
        #[derive(Default)]
        struct Group {
            blocks: Vec<Block>,
            items: Vec<Item>,
            effects: Vec<MobEffect>,
            entities: Vec<EntityType>,
            menus: Vec<MenuDeclaration>,
        }
        let namespace = |id: &str| id.split_once(':').map_or("", |(ns, _)| ns).to_owned();
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        macro_rules! group {
            ($row:expr) => {
                groups.entry(namespace(&$row.id)).or_default()
            };
        }
        for row in &self.blocks {
            group!(row).blocks.push(row.clone());
        }
        for row in &self.items {
            group!(row).items.push(row.clone());
        }
        for row in &self.effects {
            group!(row).effects.push(row.clone());
        }
        for row in &self.entities {
            group!(row).entities.push(row.clone());
        }
        for row in &self.menus {
            group!(row).menus.push(row.clone());
        }
        let mut out = Vec::with_capacity(groups.len());
        for (ns, mut group) in groups {
            group.blocks.sort_by(|a, b| a.id.cmp(&b.id));
            group.items.sort_by(|a, b| a.id.cmp(&b.id));
            group.effects.sort_by(|a, b| a.id.cmp(&b.id));
            group.entities.sort_by(|a, b| a.id.cmp(&b.id));
            group.menus.sort_by(|a, b| a.id.cmp(&b.id));
            let mut content = Content::declared(&ns);
            content
                .declare(&group.blocks, &group.items)
                .map_err(|e| format!("native declarations of {ns}: {e}"))?;
            let item_ids: BTreeSet<&str> = group
                .blocks
                .iter()
                .filter(|block| block.item)
                .map(|block| block.id.as_str())
                .chain(group.items.iter().map(|item| item.id.as_str()))
                .collect();
            let mut thrown_items = BTreeSet::new();
            for entity in group.entities {
                let id = entity.id.as_str();
                if !id.split_once(':').is_some_and(|(n, _)| n == ns) {
                    return Err(format!("native entity {id} is not an id in {ns}:"));
                }
                match &entity.item {
                    Some(item) => {
                        if !item_ids.contains(item.as_str()) {
                            return Err(format!(
                                "native entity {id}: item {item:?} is not one of {ns}'s items"
                            ));
                        }
                        if !thrown_items.insert(item.clone()) {
                            return Err(format!(
                                "native entity {id}: item {item:?} already throws another entity type"
                            ));
                        }
                    }
                    None if !entity.is_living() => {
                        return Err(format!(
                            "native entity {id}: a thrown type names the item that throws it"
                        ));
                    }
                    None => {}
                }
                content.entity_types.push(entity);
            }
            for effect in group.effects {
                content.effects.push(effect);
            }
            for menu in group.menus {
                content.menu_types.push(menu.menu_type());
            }
            out.push((ns, content));
        }
        Ok(out)
    }
}

fn bounded_json(kind: &str, id: &str, json: &str) -> Result<Vec<u8>, String> {
    if json.len() > MAX_NATIVE_JSON {
        return Err(format!(
            "native {kind} {id} exceeds {MAX_NATIVE_JSON} bytes"
        ));
    }
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| format!("native {kind} {id} is invalid JSON: {error}"))?;
    if !value.is_object() {
        return Err(format!("native {kind} {id} is not a JSON object"));
    }
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| format!("native {kind} {id} cannot be encoded: {error}"))?;
    if bytes.len() > MAX_NATIVE_JSON {
        return Err(format!(
            "native {kind} {id} exceeds {MAX_NATIVE_JSON} bytes"
        ));
    }
    Ok(bytes)
}

fn merge_rows<T, F>(kind: &str, target: &mut Vec<T>, incoming: Vec<T>, id: F) -> Result<(), String>
where
    T: Clone + PartialEq,
    F: Fn(&T) -> &str + Copy,
{
    for row in incoming {
        if let Some(existing) = target.iter().find(|existing| id(existing) == id(&row)) {
            if existing != &row {
                return Err(format!(
                    "conflicting native {kind} registration {}",
                    id(&row)
                ));
            }
        } else {
            target.push(row);
        }
    }
    target.sort_by(|a, b| id(a).cmp(id(b)));
    Ok(())
}

fn parse_f32(key: &str, value: &str) -> Result<f32, String> {
    let parsed = value
        .parse::<f32>()
        .map_err(|_| format!("native property {key} is not a number: {value:?}"))?;
    parsed
        .is_finite()
        .then_some(parsed)
        .ok_or_else(|| format!("native property {key} is not finite"))
}

fn parse_u8(key: &str, value: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("native property {key} is not an unsigned byte: {value:?}"))
}

fn parse_u16(key: &str, value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("native property {key} is not an unsigned short: {value:?}"))
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("native property {key} is not boolean: {value:?}")),
    }
}

/// A vanilla attribute default: finite and no larger than
/// [`MAX_ENTITY_ATTRIBUTE`].
fn parse_attribute(key: &str, value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("native entity property {key} is not a number: {value:?}"))?;
    if !parsed.is_finite() || !(0.0..=MAX_ENTITY_ATTRIBUTE).contains(&parsed) {
        return Err(format!(
            "native entity property {key} is a number from 0 to {MAX_ENTITY_ATTRIBUTE}"
        ));
    }
    Ok(parsed)
}

/// A menu's `results`, a comma-separated list of slot numbers.
fn parse_u16_list(key: &str, value: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("native property {key} has an empty slot"));
        }
        out.push(
            part.parse::<u16>()
                .map_err(|_| format!("native property {key}: {part:?} is not a slot"))?,
        );
        if out.len() > crate::content::MAX_MENU_SLOTS as usize {
            return Err(format!(
                "native property {key} has more than {} slots",
                crate::content::MAX_MENU_SLOTS
            ));
        }
    }
    Ok(out)
}

/// A bounded comma-separated id list property: each entry is a resource
/// location, the result is sorted and no id appears twice.
fn parse_id_list(key: &str, value: &str, max: usize) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("native property {key} has an empty id"));
        }
        NativeDeclarations::ensure_id(part)?;
        ids.push(part.to_owned());
        if ids.len() > max {
            return Err(format!("native property {key} has more than {max} ids"));
        }
    }
    ids.sort();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(format!("native property {key} repeats an id"));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
        rows.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn a_declared_block_and_item_become_content() {
        let mut declarations = NativeDeclarations::default();
        declarations
            .add_block(
                "ruby:lamp",
                "minecraft:stone",
                &properties(&[("hardness", "2.5"), ("light", "12")]),
                true,
            )
            .unwrap();
        declarations
            .add_item(
                "ruby:gem",
                "minecraft:diamond",
                &properties(&[("max_stack_size", "16"), ("rarity", "rare")]),
            )
            .unwrap();

        let mods = declarations.content_mods().unwrap();
        assert_eq!(mods.len(), 1);
        let (namespace, content) = &mods[0];
        assert_eq!(namespace, "ruby");
        assert_eq!(content.blocks.len(), 1);
        assert_eq!(content.blocks[0].id, "ruby:lamp");
        assert_eq!(content.blocks[0].template, "minecraft:stone");
        assert_eq!(content.blocks[0].properties.hardness, Some(2.5));
        assert_eq!(content.blocks[0].properties.light, Some(12));
        assert!(content.blocks[0].item);
        assert_eq!(content.items.len(), 1);
        assert_eq!(content.items[0].id, "ruby:gem");
        assert_eq!(content.items[0].max_stack_size, Some(16));
        assert_eq!(content.items[0].rarity, Some(Rarity::Rare));
    }

    #[test]
    fn declarations_are_grouped_by_namespace() {
        let mut declarations = NativeDeclarations::default();
        declarations
            .add_item("ruby:gem", "minecraft:diamond", &BTreeMap::new())
            .unwrap();
        declarations
            .add_item("jade:gem", "minecraft:emerald", &BTreeMap::new())
            .unwrap();
        let mods = declarations.content_mods().unwrap();
        let ids: Vec<&str> = mods.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["jade", "ruby"]);
    }

    #[test]
    fn a_vanilla_id_or_unknown_property_is_refused() {
        let mut declarations = NativeDeclarations::default();
        assert!(
            declarations
                .add_block("minecraft:dirt", "minecraft:stone", &BTreeMap::new(), true)
                .is_err()
        );
        assert!(
            declarations
                .add_item(
                    "ruby:gem",
                    "minecraft:diamond",
                    &properties(&[("stacks_to", "16")]),
                )
                .is_err()
        );
        assert!(declarations.items.is_empty());
    }

    #[test]
    fn an_effect_and_menu_reach_the_content() {
        let mut declarations = NativeDeclarations::default();
        declarations
            .add_effect("ruby:glow", "beneficial", 0x12_3456)
            .unwrap();
        declarations
            .add_menu(
                "ruby:crate",
                &properties(&[("slots", "27"), ("data_slots", "3"), ("results", "0,2")]),
            )
            .unwrap();
        let mods = declarations.content_mods().unwrap();
        let (_, content) = &mods[0];
        assert_eq!(content.effects.len(), 1);
        assert_eq!(content.effects[0].id, "ruby:glow");
        assert_eq!(content.effects[0].color, 0x12_3456);
        assert_eq!(content.menu_types.len(), 1);
        assert_eq!(content.menu_types[0].slots, 27);
        assert_eq!(content.menu_types[0].data_slots, 3);
        assert_eq!(content.menu_types[0].results, [0, 2]);
    }

    #[test]
    fn a_thrown_entity_needs_one_of_its_own_items() {
        let mut declarations = NativeDeclarations::default();
        declarations
            .add_entity(
                "ruby:dart",
                &properties(&[("template", "minecraft:snowball"), ("item", "ruby:dart")]),
            )
            .unwrap();
        let error = declarations.content_mods().unwrap_err();
        assert!(error.contains("not one of ruby's items"), "{error}");
    }

    #[test]
    fn merging_equal_rows_coalesces_and_conflicts_are_refused() {
        let mut first = NativeDeclarations::default();
        first
            .add_item("ruby:gem", "minecraft:diamond", &BTreeMap::new())
            .unwrap();
        let mut second = NativeDeclarations::default();
        second
            .add_item("ruby:gem", "minecraft:diamond", &BTreeMap::new())
            .unwrap();
        first.merge(second).unwrap();
        assert_eq!(first.items.len(), 1);

        let mut other = NativeDeclarations::default();
        other
            .add_item("ruby:gem", "minecraft:emerald", &BTreeMap::new())
            .unwrap();
        assert!(first.merge(other).is_err());
    }
}

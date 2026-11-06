//! The declared Java content a hosted world ships to its guests at login.
//!
//! A native Java mod's ids live in the host's registries; a guest without the
//! jars cannot resolve them, so the host sends this manifest on [`CHANNEL`]
//! right after the payload channel registration, and again when the host's
//! declaration set changes. The format is bounded and deterministic:
//! registries in one order, ids and rows sorted and deduplicated, explicit
//! counts and lengths, and trailing bytes reject.
//!
//! A v3 header carries the host's declaration epoch and a hash of the
//! declarations; a guest that already holds different non-empty declarations
//! refuses it instead of replacing its own. v1 and v2 frames still decode and
//! follow the plain merge rule, as before the header existed.
//!
//! The declaration source is the run's typed Java registry snapshot
//! (`game/java_registry.rs`), which the bootstrap publishes for every role.
//! Applying a remote manifest stores its rows in this module; the eventual
//! content model merges those stores into the command registries and the
//! menu, item, entity and block-entity lookups (the old tree kept them in
//! `mod_content`).
use super::java_registry::JavaRegistrySnapshot;
use super::lan::LanHost;
use horizon_mods::content::EntityType;
use horizon_mods::native::MenuDeclaration;
use horizon_platform::sync::Mutex;
use horizon_protocol::packets::{configuration::CustomPayload, play::Clientbound as C};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// The channel the login manifest travels on.
pub(super) const CHANNEL: &str = "horizon:java_content";

/// The manifest version this build writes: a v3 header (the declared-content
/// epoch and a content hash), then the v1 body and the v2 menu and entity
/// template sections.
pub(super) const VERSION: u16 = 3;

/// The second manifest version, still decoded: the v1 body and the bounded
/// template sections, with no header.
pub(super) const VERSION_V2: u16 = 2;

/// The first manifest version, still decoded: ids and block-entity rows only.
pub(super) const VERSION_V1: u16 = 1;

/// The largest manifest frame, the custom-payload limit.
pub(super) const MAX_FRAME: usize = 1024 * 1024;

/// The most ids one registry may carry.
pub(super) const MAX_IDS: usize = 4096;

/// The most block-entity rows one manifest may carry.
pub(super) const MAX_BLOCK_ENTITIES: usize = 4096;

/// The most menu rows, entity rows and open blocks one section may carry.
pub(super) const MAX_MENUS: usize = 1024;
pub(super) const MAX_ENTITIES: usize = 1024;
pub(super) const MAX_OPEN_BLOCKS: usize = 64;

/// The longest id or resource location one entry may name.
pub(super) const MAX_TEXT: usize = 1024;

/// The longest text one v2 section row may carry.
pub(super) const MAX_SECTION_TEXT: usize = 64;

/// The entity row's bounded spawn flags: a spawn placement, fire immunity and
/// typed attribute defaults. Everything else stays in the host's declaration.
pub(super) const ENTITY_FLAG_SPAWN: u8 = 1;
pub(super) const ENTITY_FLAG_FIRE_IMMUNE: u8 = 2;
pub(super) const ENTITY_FLAG_ATTRIBUTES: u8 = 4;
pub(super) const ENTITY_FLAG_MASK: u8 =
    ENTITY_FLAG_SPAWN | ENTITY_FLAG_FIRE_IMMUNE | ENTITY_FLAG_ATTRIBUTES;

/// The registries in wire order: blocks, items, effects, entity types, data
/// components, attachments, menus and block entity types.
const REGISTRIES: usize = 8;

/// The v2 sections, in wire order. A section is a `u8` kind and a `u16` count;
/// kinds must strictly ascend, so every manifest has one canonical byte order.
/// Kind 3 stays reserved for block and item templates: the content model
/// keeps their declared ids but no stored template row, and this build does
/// not invent one, so the section is never written.
const SECTION_MENUS: u8 = 1;
const SECTION_ENTITIES: u8 = 2;
const SECTIONS: usize = 2;

/// A declared block entity as a remote manifest lists it: one row per block,
/// with the vanilla template it behaves and saves as.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BlockEntityRow {
    pub(super) block: String,
    pub(super) id: String,
    pub(super) template: String,
    pub(super) ticker: bool,
}

/// A declared menu as a remote manifest lists it: the bounded title text (the
/// declaration's translation key, which the client resolves), its slot count
/// and the blocks that open it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct MenuRow {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) slots: u16,
    pub(super) open_blocks: Vec<String>,
}

/// A declared entity type as a remote manifest lists it: the vanilla template
/// it behaves as, its category and the bounded spawn flags.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct EntityRow {
    pub(super) id: String,
    pub(super) template: String,
    pub(super) category: String,
    pub(super) flags: u8,
}

/// Everything a content root declares, ready to travel. Ids are stored as
/// declared; [`encode`](Self::encode) sorts and deduplicates them, so the
/// same content always gives the same bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct JavaContentManifest {
    /// The declared-content epoch the host wrote in the v3 header; 0 for a v1
    /// or v2 frame, which carries no header.
    pub(super) epoch: u64,
    pub(super) blocks: Vec<String>,
    pub(super) items: Vec<String>,
    pub(super) effects: Vec<String>,
    pub(super) entity_types: Vec<String>,
    pub(super) data_components: Vec<String>,
    pub(super) attachments: Vec<String>,
    pub(super) menus: Vec<String>,
    pub(super) block_entity_types: Vec<String>,
    pub(super) block_entities: Vec<BlockEntityRow>,
    /// v2 menu templates: the bounded title text, slots and open blocks.
    pub(super) menu_rows: Vec<MenuRow>,
    /// v2 entity templates: vanilla template, category and spawn flags.
    pub(super) entity_rows: Vec<EntityRow>,
}

impl JavaContentManifest {
    /// The declarations the run's Java registry snapshot recorded: what the
    /// host sends, and what a guest would have if it had the mods. Ids and
    /// rows are capped here so a frame stays encodable whatever the root.
    pub(super) fn from_snapshot(snapshot: &JavaRegistrySnapshot) -> Self {
        let mut items = snapshot.items.clone();
        items.extend(
            snapshot
                .block_templates
                .iter()
                .filter(|block| block.item)
                .map(|block| block.id.clone()),
        );
        Self {
            epoch: 0,
            blocks: capped_ids(snapshot.blocks.clone()),
            items: capped_ids(items),
            effects: capped_ids(snapshot.effects.clone()),
            entity_types: capped_ids(snapshot.entities.clone()),
            // The current native declaration vocabulary has no data-component
            // or attachment table; the v1 body still carries the section.
            data_components: Vec::new(),
            attachments: Vec::new(),
            menus: capped_ids(snapshot.menus.clone()),
            block_entity_types: capped_ids(snapshot.block_entities.clone()),
            block_entities: capped_rows(
                snapshot
                    .block_entity_templates
                    .iter()
                    .flat_map(|declaration| {
                        declaration.valid_blocks.iter().map(|block| BlockEntityRow {
                            block: block.clone(),
                            id: declaration.id.clone(),
                            template: declaration.template.clone(),
                            ticker: declaration.ticker,
                        })
                    })
                    .collect(),
                MAX_BLOCK_ENTITIES,
            ),
            menu_rows: capped_rows(
                snapshot
                    .menu_templates
                    .iter()
                    .filter_map(menu_row)
                    .collect(),
                MAX_MENUS,
            ),
            entity_rows: capped_rows(
                snapshot
                    .entity_templates
                    .iter()
                    .filter_map(entity_row)
                    .collect(),
                MAX_ENTITIES,
            ),
        }
    }

    /// These declarations as one normalized snapshot, without touching the
    /// stores: the same ids and rows [`apply`](Self::apply) would store.
    fn declared(&self) -> Result<DeclaredContent, String> {
        DeclaredContent::normalize(
            Registries {
                blocks: self.blocks.clone(),
                items: self.items.clone(),
                effects: self.effects.clone(),
                entity_types: self.entity_types.clone(),
                data_components: self.data_components.clone(),
                attachments: self.attachments.clone(),
                menus: self.menus.clone(),
                block_entity_types: self.block_entity_types.clone(),
            },
            self.block_entities.clone(),
            self.menu_rows.clone(),
            self.entity_rows.clone(),
        )
    }

    /// The declarations' content hash, as the v3 header carries it: equal for
    /// two manifests declaring the same thing, whatever their row order.
    pub(super) fn content_hash(&self) -> Result<u64, String> {
        Ok(self.declared()?.content_hash())
    }

    /// No declaration to send; the host skips the packet.
    pub(super) fn is_empty(&self) -> bool {
        self.ids().iter().all(|ids| ids.is_empty())
            && self.block_entities.is_empty()
            && self.menu_rows.is_empty()
            && self.entity_rows.is_empty()
    }

    fn ids(&self) -> [&[String]; REGISTRIES] {
        [
            self.blocks.as_slice(),
            self.items.as_slice(),
            self.effects.as_slice(),
            self.entity_types.as_slice(),
            self.data_components.as_slice(),
            self.attachments.as_slice(),
            self.menus.as_slice(),
            self.block_entity_types.as_slice(),
        ]
    }

    /// The frame: a `u16` version 3 header (`u64` declared-content epoch and
    /// `u64` content hash), then the v1 body (per registry a `u16` count and
    /// its ids, then a `u16` row count and per row the block, declared id and
    /// template, and a `u8` ticker flag), then a `u16` section count and the
    /// menu and entity sections in kind order. The hash covers the
    /// declarations, so the same content always gives the same header.
    pub(super) fn encode(&self) -> Result<Vec<u8>, String> {
        let mut body = Writer::default();
        self.encode_body(&mut body)?;
        self.encode_sections(&mut body)?;
        let mut writer = Writer::default();
        writer.u16(VERSION)?;
        writer.u64(self.epoch)?;
        writer.u64(self.content_hash()?)?;
        writer.raw(&body.bytes)?;
        Ok(writer.bytes)
    }

    /// The v2 menu and entity template sections, in ascending kind order.
    fn encode_sections(&self, writer: &mut Writer) -> Result<(), String> {
        let mut menus = self.menu_rows.clone();
        sort_unique_menus(&mut menus)?;
        if menus.len() > MAX_MENUS {
            return Err(format!("declared menus {} exceed {MAX_MENUS}", menus.len()));
        }
        let mut entities = self.entity_rows.clone();
        sort_unique_entities(&mut entities)?;
        if entities.len() > MAX_ENTITIES {
            return Err(format!(
                "declared entities {} exceed {MAX_ENTITIES}",
                entities.len()
            ));
        }
        writer.count(SECTIONS)?;
        writer.u8(SECTION_MENUS)?;
        writer.count(menus.len())?;
        for row in &menus {
            writer.section_text(&row.id)?;
            writer.section_text(&row.title)?;
            writer.u16(row.slots)?;
            let mut blocks = row.open_blocks.clone();
            blocks.sort();
            blocks.dedup();
            if blocks.len() > MAX_OPEN_BLOCKS {
                return Err(format!(
                    "declared menu {} has {} open blocks (max {MAX_OPEN_BLOCKS})",
                    row.id,
                    blocks.len()
                ));
            }
            writer.count(blocks.len())?;
            for block in &blocks {
                writer.section_text(block)?;
            }
        }
        writer.u8(SECTION_ENTITIES)?;
        writer.count(entities.len())?;
        for row in &entities {
            writer.section_text(&row.id)?;
            writer.section_text(&row.template)?;
            writer.section_text(&row.category)?;
            if row.flags & !ENTITY_FLAG_MASK != 0 {
                return Err(format!(
                    "declared entity {} flags {:#04x} are not bounded spawn flags",
                    row.id, row.flags
                ));
            }
            writer.u8(row.flags)?;
        }
        Ok(())
    }

    /// The v1 body, shared by both versions: the eight registries and the
    /// block-entity rows.
    fn encode_body(&self, writer: &mut Writer) -> Result<(), String> {
        for ids in self.ids() {
            let mut sorted: Vec<&String> = ids.iter().collect();
            sorted.sort();
            sorted.dedup();
            if sorted.len() > MAX_IDS {
                return Err(format!("declared ids {} exceed {MAX_IDS}", sorted.len()));
            }
            writer.count(sorted.len())?;
            for id in sorted {
                writer.text(id)?;
            }
        }
        let mut rows = self.block_entities.clone();
        rows.sort();
        rows.dedup();
        if rows.len() > MAX_BLOCK_ENTITIES {
            return Err(format!(
                "declared block entities {} exceed {MAX_BLOCK_ENTITIES}",
                rows.len()
            ));
        }
        writer.count(rows.len())?;
        for row in &rows {
            writer.text(&row.block)?;
            writer.text(&row.id)?;
            writer.text(&row.template)?;
            writer.u8(u8::from(row.ticker))?;
        }
        Ok(())
    }

    /// Decode one frame, v1, v2 or v3. Every count and length is checked
    /// against its cap, text is validated as UTF-8, the ticker flag must be 0
    /// or 1, section kinds must ascend without repeats (an unknown kind
    /// rejects), and any byte after the last row rejects. A v3 header's hash
    /// must match the declarations it covers. Ids and rows come back sorted
    /// and deduplicated; a v1 frame has no templates.
    #[cfg(test)]
    pub(super) fn decode(data: &[u8]) -> Result<Self, String> {
        Self::decode_frame(data).map(|(manifest, _)| manifest)
    }

    /// [`decode`](Self::decode) plus the version the frame declared, so a
    /// caller can apply a v3 header's checks and leave a v1 or v2 frame to the
    /// plain merge rules.
    pub(super) fn decode_frame(data: &[u8]) -> Result<(Self, u16), String> {
        if data.len() > MAX_FRAME {
            return Err(format!(
                "declared Java content is {} bytes (max {MAX_FRAME})",
                data.len()
            ));
        }
        let mut reader = Reader { bytes: data, at: 0 };
        let version = reader.u16()?;
        if version != VERSION && version != VERSION_V2 && version != VERSION_V1 {
            return Err(format!(
                "declared Java content version {version} is not {VERSION}, \
                 {VERSION_V2} or {VERSION_V1}"
            ));
        }
        let mut manifest = Self::default();
        let declared_hash = if version == VERSION {
            manifest.epoch = reader.u64()?;
            Some(reader.u64()?)
        } else {
            None
        };
        manifest.decode_body(&mut reader)?;
        if version == VERSION || version == VERSION_V2 {
            manifest.decode_sections(&mut reader)?;
        }
        if reader.at != data.len() {
            return Err(format!(
                "declared Java content carries {} trailing bytes",
                data.len() - reader.at
            ));
        }
        if let Some(declared_hash) = declared_hash {
            let hash = manifest.content_hash()?;
            if hash != declared_hash {
                return Err(format!(
                    "declared Java content hash {hash:#018x} is not the header's \
                     {declared_hash:#018x}"
                ));
            }
        }
        Ok((manifest, version))
    }

    fn decode_body(&mut self, reader: &mut Reader) -> Result<(), String> {
        let fields = [
            &mut self.blocks,
            &mut self.items,
            &mut self.effects,
            &mut self.entity_types,
            &mut self.data_components,
            &mut self.attachments,
            &mut self.menus,
            &mut self.block_entity_types,
        ];
        for field in fields {
            let count = reader.count(MAX_IDS, "ids")?;
            let mut ids = Vec::with_capacity(count);
            for _ in 0..count {
                ids.push(reader.text()?);
            }
            ids.sort();
            ids.dedup();
            *field = ids;
        }
        let count = reader.count(MAX_BLOCK_ENTITIES, "block entity rows")?;
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            let block = reader.text()?;
            let id = reader.text()?;
            let template = reader.text()?;
            let ticker = match reader.u8()? {
                0 => false,
                1 => true,
                other => {
                    return Err(format!(
                        "declared Java content ticker flag {other} is not 0 or 1"
                    ));
                }
            };
            rows.push(BlockEntityRow {
                block,
                id,
                template,
                ticker,
            });
        }
        rows.sort();
        rows.dedup();
        self.block_entities = rows;
        Ok(())
    }

    fn decode_sections(&mut self, reader: &mut Reader) -> Result<(), String> {
        let sections = reader.count(SECTIONS, "sections")?;
        let mut last = 0u8;
        for _ in 0..sections {
            let kind = reader.u8()?;
            if kind <= last {
                return Err(format!(
                    "declared Java content section {kind} is out of order"
                ));
            }
            last = kind;
            match kind {
                SECTION_MENUS => {
                    let count = reader.count(MAX_MENUS, "menus")?;
                    let mut rows = Vec::with_capacity(count);
                    for _ in 0..count {
                        let id = reader.section_text()?;
                        let title = reader.section_text()?;
                        let slots = reader.u16()?;
                        let blocks = reader.count(MAX_OPEN_BLOCKS, "menu open blocks")?;
                        let mut open_blocks = Vec::with_capacity(blocks);
                        for _ in 0..blocks {
                            open_blocks.push(reader.section_text()?);
                        }
                        rows.push(MenuRow {
                            id,
                            title,
                            slots,
                            open_blocks,
                        });
                    }
                    sort_unique_menus(&mut rows)?;
                    self.menu_rows = rows;
                }
                SECTION_ENTITIES => {
                    let count = reader.count(MAX_ENTITIES, "entities")?;
                    let mut rows = Vec::with_capacity(count);
                    for _ in 0..count {
                        let id = reader.section_text()?;
                        let template = reader.section_text()?;
                        let category = reader.section_text()?;
                        let flags = reader.u8()?;
                        if flags & !ENTITY_FLAG_MASK != 0 {
                            return Err(format!(
                                "declared entity {id} flags {flags:#04x} are not bounded spawn flags"
                            ));
                        }
                        rows.push(EntityRow {
                            id,
                            template,
                            category,
                            flags,
                        });
                    }
                    sort_unique_entities(&mut rows)?;
                    self.entity_rows = rows;
                }
                other => {
                    return Err(format!("declared Java content has unknown section {other}"));
                }
            }
        }
        Ok(())
    }

    /// Store this manifest into the process's declaration stores, returning
    /// how many declarations it applied. See [`apply_frame`](Self::apply_frame)
    /// for the v3 conflict rule.
    #[cfg(test)]
    fn apply(&self) -> Result<usize, String> {
        let incoming = self.declared()?;
        let mut store = store();
        let current = local_or_registry(store.as_ref());
        let merged = merge(current, incoming)?;
        let count = merged.count();
        *store = Some(merged);
        STORE_EPOCH.fetch_add(1, Ordering::Release);
        Ok(count)
    }

    /// Apply a decoded frame of `version`, atomically: the local declarations
    /// are read, checked and replaced behind one lock. A v1 or v2 frame
    /// follows the plain merge rule. A v3 frame is refused, leaving the stores
    /// untouched, when the process already holds different non-empty
    /// declarations; the header hash was already checked by
    /// [`decode_frame`](Self::decode_frame).
    fn apply_frame(&self, version: u16) -> Result<Outcome, String> {
        let incoming = self.declared()?;
        let mut store = store();
        let current = local_or_registry(store.as_ref());
        if version == VERSION && !incoming.is_empty() {
            let local = current.content_hash();
            let incoming_hash = incoming.content_hash();
            if local != 0 && local != incoming_hash {
                return Ok(Outcome::Refused(refusal_reason(
                    self.epoch,
                    local,
                    incoming_hash,
                )));
            }
        }
        let merged = merge(current, incoming)?;
        let count = merged.count();
        *store = Some(merged);
        STORE_EPOCH.fetch_add(1, Ordering::Release);
        if version == VERSION {
            APPLIED_EPOCH.store(self.epoch, Ordering::Release);
        }
        Ok(Outcome::Applied(count))
    }
}

/// The eight id registries, in wire order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Registries {
    blocks: Vec<String>,
    items: Vec<String>,
    effects: Vec<String>,
    entity_types: Vec<String>,
    data_components: Vec<String>,
    attachments: Vec<String>,
    menus: Vec<String>,
    block_entity_types: Vec<String>,
}

impl Registries {
    fn is_empty(&self) -> bool {
        [
            &self.blocks,
            &self.items,
            &self.effects,
            &self.entity_types,
            &self.data_components,
            &self.attachments,
            &self.menus,
            &self.block_entity_types,
        ]
        .iter()
        .all(|ids| ids.is_empty())
    }
}

/// The declaration set the process holds: the command registries, the extra
/// registries, the block entities keyed by block, and the bounded menu and
/// entity templates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DeclaredContent {
    ids: Registries,
    block_entities: BTreeMap<String, BlockEntityRow>,
    menus: BTreeMap<String, MenuRow>,
    entities: BTreeMap<String, EntityRow>,
}

impl DeclaredContent {
    /// Normalize one declaration batch: ids sorted and bounded, rows keyed
    /// and sorted, and two different rows for one key rejected rather than
    /// picking a winner.
    fn normalize(
        ids: Registries,
        rows: Vec<BlockEntityRow>,
        menus: Vec<MenuRow>,
        entities: Vec<EntityRow>,
    ) -> Result<Self, String> {
        Ok(Self {
            ids: Registries {
                blocks: bounded_ids(ids.blocks)?,
                items: bounded_ids(ids.items)?,
                effects: bounded_ids(ids.effects)?,
                entity_types: bounded_ids(ids.entity_types)?,
                data_components: bounded_ids(ids.data_components)?,
                attachments: bounded_ids(ids.attachments)?,
                menus: bounded_ids(ids.menus)?,
                block_entity_types: bounded_ids(ids.block_entity_types)?,
            },
            block_entities: block_entity_map(rows)?,
            menus: menu_map(menus)?,
            entities: entity_map(entities)?,
        })
    }

    /// The declarations of one registry snapshot, without touching the
    /// stores. Normalization here cannot fail on rows the snapshot builder
    /// already coalesced; a row too long for the wire section is left out
    /// while its id still travels.
    fn from_snapshot(snapshot: &JavaRegistrySnapshot) -> Self {
        match JavaContentManifest::from_snapshot(snapshot).declared() {
            Ok(content) => content,
            Err(error) => {
                log::warn!("Horizon: declared Java content is not representable: {error}");
                Self::default()
            }
        }
    }

    fn from_stored(content: &Self) -> JavaContentManifest {
        JavaContentManifest {
            epoch: 0,
            blocks: content.ids.blocks.clone(),
            items: content.ids.items.clone(),
            effects: content.ids.effects.clone(),
            entity_types: content.ids.entity_types.clone(),
            data_components: content.ids.data_components.clone(),
            attachments: content.ids.attachments.clone(),
            menus: content.ids.menus.clone(),
            block_entity_types: content.ids.block_entity_types.clone(),
            block_entities: content.block_entities.values().cloned().collect(),
            menu_rows: content.menus.values().cloned().collect(),
            entity_rows: content.entities.values().cloned().collect(),
        }
    }

    /// Nothing to resolve: the host has no declared content.
    fn is_empty(&self) -> bool {
        self.ids.is_empty()
            && self.block_entities.is_empty()
            && self.menus.is_empty()
            && self.entities.is_empty()
    }

    /// How many declarations travel: ids, extra ids, block-entity rows and
    /// template rows.
    fn count(&self) -> usize {
        let ids: usize = [
            &self.ids.blocks,
            &self.ids.items,
            &self.ids.effects,
            &self.ids.entity_types,
            &self.ids.data_components,
            &self.ids.attachments,
            &self.ids.menus,
            &self.ids.block_entity_types,
        ]
        .iter()
        .map(|ids| ids.len())
        .sum();
        ids + self.block_entities.len() + self.menus.len() + self.entities.len()
    }

    /// A bounded hash over these declarations in one canonical order: the
    /// eight registries' sorted ids, then the block-entity, menu and entity
    /// rows by key. A v3 login manifest carries it in its header, so a guest
    /// can tell "the same declarations" from "different ones" without
    /// comparing every row. Empty content hashes to 0, so "nothing declared"
    /// never compares equal to a real declaration set.
    ///
    /// The layout matches the old `mod_content` hasher, so a host and a guest
    /// on either build agree on the same declarations.
    fn content_hash(&self) -> u64 {
        if self.is_empty() {
            return 0;
        }
        let mut hasher = ContentHasher::new();
        let registries: [&[String]; 8] = [
            &self.ids.blocks,
            &self.ids.items,
            &self.ids.effects,
            &self.ids.entity_types,
            &self.ids.data_components,
            &self.ids.attachments,
            &self.ids.menus,
            &self.ids.block_entity_types,
        ];
        for (index, ids) in registries.iter().enumerate() {
            hasher.tag(index as u8);
            let mut ids: Vec<&String> = ids.iter().collect();
            ids.sort();
            ids.dedup();
            hasher.count(ids.len());
            for id in ids {
                hasher.text(id);
            }
        }
        hasher.tag(8);
        hasher.count(self.block_entities.len());
        for (block, row) in &self.block_entities {
            hasher.text(block);
            hasher.text(&row.id);
            hasher.text(&row.template);
            hasher.byte(u8::from(row.ticker));
        }
        hasher.tag(9);
        hasher.count(self.menus.len());
        for (id, row) in &self.menus {
            hasher.text(id);
            hasher.text(&row.title);
            hasher.count(row.slots as usize);
            let mut blocks = row.open_blocks.clone();
            blocks.sort();
            blocks.dedup();
            hasher.count(blocks.len());
            for block in &blocks {
                hasher.text(block);
            }
        }
        hasher.tag(10);
        hasher.count(self.entities.len());
        for (id, row) in &self.entities {
            hasher.text(id);
            hasher.text(&row.template);
            hasher.text(&row.category);
            hasher.byte(row.flags);
        }
        hasher.finish()
    }
}

/// The canonical byte stream [`DeclaredContent::content_hash`] hashes:
/// tagged, length-prefixed fields in one order. The first 8 bytes of SHA-256
/// are enough to tell declaration sets apart on the wire, and a fixed width
/// keeps the v3 header bounded.
struct ContentHasher(Sha256);

impl ContentHasher {
    fn new() -> Self {
        Self(Sha256::new())
    }

    fn tag(&mut self, tag: u8) {
        self.0.update([tag]);
    }

    fn count(&mut self, count: usize) {
        self.0.update((count as u64).to_be_bytes());
    }

    fn text(&mut self, text: &str) {
        self.0.update((text.len() as u64).to_be_bytes());
        self.0.update(text.as_bytes());
    }

    fn byte(&mut self, byte: u8) {
        self.0.update([byte]);
    }

    fn finish(self) -> u64 {
        let digest = self.0.finalize();
        u64::from_be_bytes(digest[..8].try_into().unwrap_or_default())
    }
}

/// The process's applied declarations. `None` means no manifest was ever
/// applied: the run's own Java registry is then what the process holds.
static STORE: Mutex<Option<DeclaredContent>> = Mutex::new(None);

fn store() -> std::sync::MutexGuard<'static, Option<DeclaredContent>> {
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the process holds right now: the applied manifest once one arrived,
/// otherwise the declarations the run's Java registry recorded.
fn local_or_registry(store: Option<&DeclaredContent>) -> DeclaredContent {
    match store {
        Some(content) => content.clone(),
        None => DeclaredContent::from_snapshot(&super::java_registry::snapshot()),
    }
}

/// The declarations the process holds, cloned.
fn local_declarations() -> DeclaredContent {
    let store = store();
    local_or_registry(store.as_ref())
}

/// Incremented after the declaration stores are replaced. Diagnostics use it;
/// a host's manifest epoch is the snapshot generation below.
static STORE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The host's last sent snapshot: `(content hash, epoch)`.
static HOST_SNAPSHOT: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// The host-manifest epoch counter.
static HOST_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The manifest a host sends right now. Its epoch moves when the declarations
/// change, so the resend sees one snapshot per content change and not one per
/// tick.
pub(super) fn host_manifest() -> JavaContentManifest {
    let mut manifest = DeclaredContent::from_stored(&local_declarations());
    let hash = manifest.content_hash().unwrap_or(0);
    let mut snapshot = HOST_SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
    let epoch = match *snapshot {
        Some((seen, epoch)) if seen == hash => epoch,
        _ => {
            let epoch = HOST_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
            *snapshot = Some((hash, epoch));
            epoch
        }
    };
    manifest.epoch = epoch;
    manifest
}

/// The declared-content epoch of the last v3 manifest this client applied, 0
/// until one is. A later post-login re-send uses it to tell "the host's
/// declarations changed" from "the same content again".
static APPLIED_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The declared-content epoch of the last v3 manifest this client applied.
#[cfg(test)]
fn applied_epoch() -> u64 {
    APPLIED_EPOCH.load(Ordering::Acquire)
}

/// What one received manifest did to the declarations this process holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The manifest was stored; this many declarations resolve now.
    Applied(usize),
    /// The frame was well formed but the client already holds a different,
    /// non-empty declaration set; the local content is unchanged and the
    /// bounded one-line reason is for the log.
    Refused(String),
}

/// Decode one received frame and store it; the client's whole handler. A v3
/// header is checked against the local declarations first, so a guest with its
/// own mods keeps them.
pub(super) fn apply(data: &[u8]) -> Result<Outcome, String> {
    let (manifest, version) = JavaContentManifest::decode_frame(data)?;
    manifest.apply_frame(version)
}

/// The longest refusal line [`refusal_reason`] builds; the log and any future
/// chat line stay bounded whatever the host sends.
pub(super) const MAX_REFUSAL: usize = 160;

/// One bounded, single-line reason for keeping the local declarations.
fn refusal_reason(epoch: u64, local: u64, incoming: u64) -> String {
    let reason = format!(
        "server declared Java content {incoming:#018x} (epoch {epoch}) but this client \
         already holds {local:#018x}; keeping the local content"
    );
    // Control characters never reach the log line, and it stays within
    // `MAX_REFUSAL` bytes whatever the host's numbers were.
    let mut bounded = String::with_capacity(MAX_REFUSAL);
    for c in reason.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if bounded.len() + c.len_utf8() > MAX_REFUSAL {
            break;
        }
        bounded.push(c);
    }
    bounded
}

static REJECTED: AtomicBool = AtomicBool::new(false);

/// Log a rejected manifest once: a broken or conflicting host should not
/// fill the log with one line per packet.
pub(super) fn warn_once(error: &str) {
    if !REJECTED.swap(true, Ordering::Relaxed) {
        log::warn!("Horizon: declared Java content was rejected: {error}");
    }
}

static REFUSED: AtomicBool = AtomicBool::new(false);

/// Log a refused manifest once: a host re-sending conflicting declarations
/// should not fill the log one line per frame.
pub(super) fn warn_refused_once(reason: &str) {
    if !REFUSED.swap(true, Ordering::Relaxed) {
        log::warn!("Horizon: kept the local Java content: {reason}");
    }
}

/// What one guest last received: `(epoch, content hash)`.
static SENT: Mutex<BTreeMap<u32, (u64, u64)>> = Mutex::new(BTreeMap::new());

/// Whether a guest's copy is current: the same epoch needs nothing, a new
/// epoch with the same content is absorbed, and anything else sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContentSend {
    Skip,
    Absorb,
    Send,
}

fn content_action(sent: Option<(u64, u64)>, epoch: u64, hash: u64) -> ContentSend {
    match sent {
        Some((sent_epoch, _)) if sent_epoch == epoch => ContentSend::Skip,
        Some((_, sent_hash)) if sent_hash == hash => ContentSend::Absorb,
        _ => ContentSend::Send,
    }
}

/// The `horizon:java_content` packet a manifest travels as, or `None` when
/// there is nothing to send. A malformed manifest is logged and skipped.
fn manifest_packet(manifest: &JavaContentManifest) -> Option<C> {
    if manifest.is_empty() {
        return None;
    }
    match manifest.encode() {
        Ok(data) => Some(C::CustomPayload(CustomPayload {
            channel: CHANNEL.to_owned(),
            data,
        })),
        Err(error) => {
            log::warn!("Horizon: declared Java content was not sent: {error}");
            None
        }
    }
}

/// Send the host's manifest to a guest in the login burst, before the Login
/// packet and the first chunks, and record it as that guest's copy.
pub(super) fn send_login_manifest(host: &LanHost, conn: u32) {
    let manifest = host_manifest();
    let hash = manifest.content_hash().unwrap_or(0);
    if let Some(guest) = host.hub.guest(conn)
        && let Some(packet) = manifest_packet(&manifest)
    {
        guest.send(&packet);
    }
    SENT.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conn, (manifest.epoch, hash));
}

/// The tick's manifest work: every placed guest whose copy is not the host's
/// current declarations gets the v3 frame. The map is pruned to the guests
/// still connected, so a rejoining connection starts without a copy.
pub(super) fn tick_java_content(host: &LanHost) {
    let manifest = host_manifest();
    let hash = manifest.content_hash().unwrap_or(0);
    let packet = manifest_packet(&manifest);
    let mut sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
    sent.retain(|conn, _| host.hub.guest(*conn).is_some());
    for guest in host.hub.guests.iter().filter(|guest| guest.placed) {
        match content_action(sent.get(&guest.conn).copied(), manifest.epoch, hash) {
            ContentSend::Skip => {}
            ContentSend::Absorb => {
                sent.insert(guest.conn, (manifest.epoch, hash));
            }
            ContentSend::Send => {
                if let Some(packet) = &packet {
                    guest.send(packet);
                }
                sent.insert(guest.conn, (manifest.epoch, hash));
            }
        }
    }
}

/// Sort rows by id and drop exact repeats; a second, different row for one id
/// is ambiguous and rejects rather than picking a winner.
fn sort_unique_menus(rows: &mut Vec<MenuRow>) -> Result<(), String> {
    for row in rows.iter_mut() {
        row.open_blocks.sort();
        row.open_blocks.dedup();
    }
    rows.sort();
    rows.dedup();
    if let Some(pair) = rows.windows(2).find(|pair| pair[0].id == pair[1].id) {
        return Err(format!("declared menu {} is listed twice", pair[0].id));
    }
    Ok(())
}

fn sort_unique_entities(rows: &mut Vec<EntityRow>) -> Result<(), String> {
    rows.sort();
    rows.dedup();
    if let Some(pair) = rows.windows(2).find(|pair| pair[0].id == pair[1].id) {
        return Err(format!("declared entity {} is listed twice", pair[0].id));
    }
    Ok(())
}

/// Sort, deduplicate and bound one registry's ids.
fn bounded_ids(mut ids: Vec<String>) -> Result<Vec<String>, String> {
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_IDS {
        return Err(format!("declared ids {} exceed {MAX_IDS}", ids.len()));
    }
    Ok(ids)
}

/// Sort, deduplicate and truncate one host id list: a snapshot past the wire
/// cap gives up its tail rather than failing to send anything.
fn capped_ids(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids.dedup();
    ids.truncate(MAX_IDS);
    ids
}

/// Sort, deduplicate and truncate one host row list.
fn capped_rows<T: Ord>(mut rows: Vec<T>, max: usize) -> Vec<T> {
    rows.sort();
    rows.dedup();
    rows.truncate(max);
    rows
}

fn text_fits(text: &str) -> bool {
    text.len() <= MAX_SECTION_TEXT
}

fn menu_row(declaration: &MenuDeclaration) -> Option<MenuRow> {
    let row = MenuRow {
        id: declaration.id.clone(),
        title: declaration.title_key.clone().unwrap_or_default(),
        slots: declaration.slots,
        open_blocks: declaration.open_blocks.clone(),
    };
    menu_row_fits(&row).then_some(row)
}

fn menu_row_fits(row: &MenuRow) -> bool {
    let mut open_blocks = row.open_blocks.clone();
    open_blocks.sort();
    open_blocks.dedup();
    text_fits(&row.id)
        && text_fits(&row.title)
        && open_blocks.len() <= MAX_OPEN_BLOCKS
        && open_blocks.iter().all(|block| text_fits(block))
}

fn entity_row(entity: &EntityType) -> Option<EntityRow> {
    let mut flags = 0;
    // The current native vocabulary has no spawn-placement or fire-immunity
    // key; a living template is what a spawn egg can place, and its typed
    // attribute overrides travel as their flag.
    if horizon_mods::content::LIVING_TEMPLATES.contains(&entity.template.as_str()) {
        flags |= ENTITY_FLAG_SPAWN;
    }
    if !entity.attributes.is_empty() {
        flags |= ENTITY_FLAG_ATTRIBUTES;
    }
    let row = EntityRow {
        id: entity.id.clone(),
        template: entity.template.clone(),
        category: String::new(),
        flags,
    };
    entity_row_fits(&row).then_some(row)
}

fn entity_row_fits(row: &EntityRow) -> bool {
    text_fits(&row.id) && text_fits(&row.template) && text_fits(&row.category)
}

/// Rebuild the block-keyed map from wire rows. When two declarations name one
/// block the lower id wins, so content order cannot change which entity is
/// built.
fn block_entity_map(rows: Vec<BlockEntityRow>) -> Result<BTreeMap<String, BlockEntityRow>, String> {
    let mut out: BTreeMap<String, BlockEntityRow> = BTreeMap::new();
    for row in rows {
        match out.get_mut(&row.block) {
            Some(existing) if existing.id <= row.id => {}
            Some(existing) => *existing = row,
            None => {
                out.insert(row.block.clone(), row);
            }
        }
    }
    if out.len() > MAX_BLOCK_ENTITIES {
        return Err(format!(
            "declared block entities {} exceed {MAX_BLOCK_ENTITIES}",
            out.len()
        ));
    }
    Ok(out)
}

fn menu_map(rows: Vec<MenuRow>) -> Result<BTreeMap<String, MenuRow>, String> {
    let mut out: BTreeMap<String, MenuRow> = BTreeMap::new();
    for row in rows {
        let mut row = row;
        row.open_blocks.sort();
        row.open_blocks.dedup();
        match out.get(&row.id) {
            Some(existing) if existing == &row => {}
            Some(_) => return Err(format!("declared menu {} is listed twice", row.id)),
            None => {
                if out.len() >= MAX_MENUS {
                    return Err(format!(
                        "declared menus {} exceed {MAX_MENUS}",
                        out.len() + 1
                    ));
                }
                out.insert(row.id.clone(), row);
            }
        }
    }
    Ok(out)
}

fn entity_map(rows: Vec<EntityRow>) -> Result<BTreeMap<String, EntityRow>, String> {
    let mut out: BTreeMap<String, EntityRow> = BTreeMap::new();
    for row in rows {
        if row.flags & !ENTITY_FLAG_MASK != 0 {
            return Err(format!(
                "declared entity {} flags {:#04x} are not bounded spawn flags",
                row.id, row.flags
            ));
        }
        match out.get(&row.id) {
            Some(existing) if existing == &row => {}
            Some(_) => return Err(format!("declared entity {} is listed twice", row.id)),
            None => {
                if out.len() >= MAX_ENTITIES {
                    return Err(format!(
                        "declared entities {} exceed {MAX_ENTITIES}",
                        out.len() + 1
                    ));
                }
                out.insert(row.id.clone(), row);
            }
        }
    }
    Ok(out)
}

/// An empty current set takes the incoming one, an identical set is a no-op,
/// and two different non-empty sets conflict: a guest that has its own mods
/// never has them silently replaced.
fn merge(current: DeclaredContent, incoming: DeclaredContent) -> Result<DeclaredContent, String> {
    if incoming.is_empty() {
        return Ok(current);
    }
    if current.is_empty() || current == incoming {
        return Ok(incoming);
    }
    Err("declared Java content conflicts with the content already loaded".to_owned())
}

/// Reset every declaration store; tests that write the process-wide stores
/// start from here.
#[cfg(test)]
fn clear_declared_java_content() {
    *store() = Some(DeclaredContent::default());
    *HOST_SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    HOST_EPOCH.store(0, Ordering::Release);
    STORE_EPOCH.store(0, Ordering::Release);
    APPLIED_EPOCH.store(0, Ordering::Release);
    SENT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The declarations the process holds, for tests.
#[cfg(test)]
fn stored() -> DeclaredContent {
    local_declarations()
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn reserve(&self, extra: usize) -> Result<(), String> {
        if self.bytes.len().saturating_add(extra) > MAX_FRAME {
            return Err(format!("declared Java content exceeds {MAX_FRAME} bytes"));
        }
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), String> {
        self.reserve(1)?;
        self.bytes.push(value);
        Ok(())
    }

    fn u16(&mut self, value: u16) -> Result<(), String> {
        self.reserve(2)?;
        self.bytes.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn u64(&mut self, value: u64) -> Result<(), String> {
        self.reserve(8)?;
        self.bytes.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn count(&mut self, count: usize) -> Result<(), String> {
        let count = u16::try_from(count)
            .map_err(|_| format!("declared Java content count {count} overflows"))?;
        self.u16(count)
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        self.text_max(text, MAX_TEXT)
    }

    fn section_text(&mut self, text: &str) -> Result<(), String> {
        self.text_max(text, MAX_SECTION_TEXT)
    }

    fn text_max(&mut self, text: &str, max: usize) -> Result<(), String> {
        let len = text.len();
        if len > max {
            return Err(format!(
                "declared Java content text is {len} bytes (max {max})"
            ));
        }
        self.count(len)?;
        self.reserve(len)?;
        self.bytes.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| "declared Java content is truncated".to_owned())?;
        self.at += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let hi = self.u8()?;
        let lo = self.u8()?;
        Ok(u16::from_be_bytes([hi, lo]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let mut bytes = [0u8; 8];
        for byte in &mut bytes {
            *byte = self.u8()?;
        }
        Ok(u64::from_be_bytes(bytes))
    }

    fn count(&mut self, max: usize, what: &str) -> Result<usize, String> {
        let count = self.u16()? as usize;
        if count > max {
            return Err(format!(
                "declared Java content lists {count} {what} (max {max})"
            ));
        }
        Ok(count)
    }

    fn text(&mut self) -> Result<String, String> {
        self.text_max(MAX_TEXT)
    }

    fn section_text(&mut self) -> Result<String, String> {
        self.text_max(MAX_SECTION_TEXT)
    }

    fn text_max(&mut self, max: usize) -> Result<String, String> {
        let len = self.u16()? as usize;
        if len > max {
            return Err(format!(
                "declared Java content text is {len} bytes (max {max})"
            ));
        }
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| "declared Java content length overflows".to_owned())?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| "declared Java content is truncated".to_owned())?;
        self.at = end;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| "declared Java content text is not UTF-8".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_mods::content::Block;
    use horizon_mods::native::{BlockEntityDeclaration, MenuDeclaration, NativeDeclarations};
    use horizon_platform::sync::Mutex;

    /// The stores are process-wide; the tests that write them take turns.
    static STORE_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn manifest() -> JavaContentManifest {
        JavaContentManifest {
            epoch: 0,
            blocks: vec!["ruby:block".into()],
            items: vec!["ruby:block".into(), "ruby:gem".into()],
            effects: vec!["ruby:glow".into()],
            entity_types: vec!["ruby:beast".into()],
            data_components: vec!["ruby:charge".into()],
            attachments: vec!["ruby:mark".into()],
            menus: vec!["ruby:crate".into()],
            block_entity_types: vec!["ruby:crate_entity".into()],
            block_entities: vec![
                BlockEntityRow {
                    block: "ruby:box".into(),
                    id: "ruby:crate_entity".into(),
                    template: "minecraft:barrel".into(),
                    ticker: true,
                },
                BlockEntityRow {
                    block: "ruby:crate".into(),
                    id: "ruby:crate_entity".into(),
                    template: "minecraft:barrel".into(),
                    ticker: true,
                },
            ],
            menu_rows: vec![MenuRow {
                id: "ruby:crate".into(),
                title: "container.ruby.crate".into(),
                slots: 27,
                open_blocks: vec!["ruby:box".into(), "ruby:crate".into()],
            }],
            entity_rows: vec![EntityRow {
                id: "ruby:beast".into(),
                template: "minecraft:pig".into(),
                category: "creature".into(),
                flags: ENTITY_FLAG_SPAWN | ENTITY_FLAG_FIRE_IMMUNE,
            }],
        }
    }

    fn row(block: &str, id: &str, template: &str) -> BlockEntityRow {
        BlockEntityRow {
            block: block.into(),
            id: id.into(),
            template: template.into(),
            ticker: false,
        }
    }

    #[test]
    fn every_registry_and_block_entities_round_trip() {
        let source = manifest();
        let bytes = source.encode().unwrap();
        let decoded = JavaContentManifest::decode(&bytes).unwrap();
        assert_eq!(decoded, source);
        // Every registry came back sorted and deduplicated.
        assert_eq!(decoded.items, ["ruby:block", "ruby:gem"]);
        assert_eq!(decoded.blocks, ["ruby:block"]);
        assert_eq!(decoded.effects, ["ruby:glow"]);
        assert_eq!(decoded.entity_types, ["ruby:beast"]);
        assert_eq!(decoded.data_components, ["ruby:charge"]);
        assert_eq!(decoded.attachments, ["ruby:mark"]);
        assert_eq!(decoded.menus, ["ruby:crate"]);
        assert_eq!(decoded.block_entity_types, ["ruby:crate_entity"]);
        assert_eq!(decoded.block_entities.len(), 2);
        // The v2 template sections came back too.
        assert_eq!(decoded.menu_rows.len(), 1);
        assert_eq!(decoded.menu_rows[0].title, "container.ruby.crate");
        assert_eq!(decoded.menu_rows[0].slots, 27);
        assert_eq!(decoded.entity_rows[0].template, "minecraft:pig");
        assert_eq!(decoded.entity_rows[0].category, "creature");
    }

    /// A v1 frame still decodes: ids and block-entity rows, no templates.
    #[test]
    fn v1_frames_decode_without_templates() {
        let source = manifest();
        let mut writer = Writer::default();
        writer.u16(VERSION_V1).unwrap();
        source.encode_body(&mut writer).unwrap();
        let (decoded, version) = JavaContentManifest::decode_frame(&writer.bytes).unwrap();
        assert_eq!(version, VERSION_V1);
        assert_eq!(decoded.blocks, ["ruby:block"]);
        assert_eq!(decoded.block_entities.len(), 2);
        assert!(decoded.menu_rows.is_empty());
        assert!(decoded.entity_rows.is_empty());
        assert_eq!(decoded.epoch, 0);
    }

    /// A v2 frame still decodes exactly as before: body and template sections,
    /// no header and therefore epoch 0.
    #[test]
    fn v2_frames_decode_without_a_header() {
        let source = manifest();
        let mut writer = Writer::default();
        writer.u16(VERSION_V2).unwrap();
        source.encode_body(&mut writer).unwrap();
        source.encode_sections(&mut writer).unwrap();
        let (decoded, version) = JavaContentManifest::decode_frame(&writer.bytes).unwrap();
        assert_eq!(version, VERSION_V2);
        assert_eq!(decoded.epoch, 0);
        assert_eq!(decoded.menu_rows, source.menu_rows);
        assert_eq!(decoded.entity_rows, source.entity_rows);
        assert_eq!(JavaContentManifest::decode(&writer.bytes).unwrap(), source);
    }

    #[test]
    fn empty_manifest_round_trips_and_is_skipped() {
        let empty = JavaContentManifest::default();
        assert!(empty.is_empty());
        let decoded = JavaContentManifest::decode(&empty.encode().unwrap()).unwrap();
        assert_eq!(decoded, empty);
        assert!(!manifest().is_empty());
    }

    #[test]
    fn encoding_is_deterministic_whatever_the_order() {
        let mut shuffled = manifest();
        shuffled.items = vec!["ruby:gem".into(), "ruby:block".into(), "ruby:gem".into()];
        shuffled.block_entities.reverse();
        shuffled.menu_rows[0].open_blocks.reverse();
        shuffled.menu_rows.push(shuffled.menu_rows[0].clone());
        shuffled.entity_rows.push(shuffled.entity_rows[0].clone());
        assert_eq!(
            manifest().encode().unwrap(),
            shuffled.encode().unwrap(),
            "same declarations, same bytes"
        );
    }

    #[test]
    fn truncated_and_trailing_frames_reject() {
        let bytes = manifest().encode().unwrap();
        for len in [0, 1, 2, bytes.len() - 1] {
            assert!(
                JavaContentManifest::decode(&bytes[..len]).is_err(),
                "a {len}-byte prefix must not decode"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(JavaContentManifest::decode(&trailing).is_err());
    }

    #[test]
    fn oversize_counts_and_text_reject() {
        let mut too_many = manifest();
        too_many.blocks = (0..=MAX_IDS).map(|i| format!("ruby:b{i}")).collect();
        assert!(too_many.encode().is_err());

        let mut too_long = manifest();
        too_long.effects = vec!["r".repeat(MAX_TEXT + 1)];
        assert!(too_long.encode().is_err());

        // A hand-built frame may lie about a count or length.
        let mut frame = Vec::new();
        frame.extend_from_slice(&VERSION_V1.to_be_bytes());
        frame.extend_from_slice(&(u16::try_from(MAX_IDS + 1).unwrap()).to_be_bytes());
        assert!(JavaContentManifest::decode(&frame).is_err());

        let mut frame = Vec::new();
        frame.extend_from_slice(&VERSION_V1.to_be_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes()); // blocks
        frame.extend_from_slice(&1u16.to_be_bytes()); // items
        frame.extend_from_slice(&(u16::try_from(MAX_TEXT + 1).unwrap()).to_be_bytes());
        assert!(JavaContentManifest::decode(&frame).is_err());
    }

    #[test]
    fn invalid_utf8_version_and_ticker_flags_reject() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(VERSION + 1).to_be_bytes());
        assert!(JavaContentManifest::decode(&frame).is_err());

        let mut frame = Vec::new();
        frame.extend_from_slice(&VERSION_V1.to_be_bytes());
        for _ in 0..REGISTRIES {
            frame.extend_from_slice(&0u16.to_be_bytes());
        }
        frame.extend_from_slice(&1u16.to_be_bytes()); // one row
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.push(0xff); // block is not UTF-8
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.push(b'a');
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.push(b'a');
        frame.push(0);
        assert!(JavaContentManifest::decode(&frame).is_err());

        let mut frame = Vec::new();
        frame.extend_from_slice(&VERSION_V1.to_be_bytes());
        for _ in 0..REGISTRIES {
            frame.extend_from_slice(&0u16.to_be_bytes());
        }
        frame.extend_from_slice(&1u16.to_be_bytes());
        for _ in 0..3 {
            frame.extend_from_slice(&1u16.to_be_bytes());
            frame.push(b'a');
        }
        frame.push(2); // not a boolean
        assert!(JavaContentManifest::decode(&frame).is_err());
    }

    #[test]
    fn v2_sections_are_bounded_and_reject_bad_frames() {
        let source = manifest();
        // Text, open blocks and flags are capped.
        let mut long = source.clone();
        long.menu_rows[0].title = "x".repeat(MAX_SECTION_TEXT + 1);
        assert!(long.encode().is_err());
        let mut many = source.clone();
        many.menu_rows[0].open_blocks = (0..=MAX_OPEN_BLOCKS)
            .map(|i| format!("ruby:b{i}"))
            .collect();
        assert!(many.encode().is_err());
        let mut bad_flags = source.clone();
        bad_flags.entity_rows[0].flags = 0xff;
        assert!(bad_flags.encode().is_err());
        // Two different rows for one id are ambiguous.
        let mut twice = source.clone();
        let mut other = twice.menu_rows[0].clone();
        other.title = "container.ruby.other".into();
        twice.menu_rows.push(other);
        assert!(twice.encode().is_err());

        // Unknown, repeated and out-of-order section kinds reject.
        let body = |writer: &mut Writer| {
            writer.u16(VERSION_V2).unwrap();
            source.encode_body(writer).unwrap();
        };
        let mut frame = Writer::default();
        body(&mut frame);
        frame.count(1).unwrap();
        frame.u8(9).unwrap();
        assert!(JavaContentManifest::decode(&frame.bytes).is_err());
        let mut frame = Writer::default();
        body(&mut frame);
        frame.count(2).unwrap();
        frame.u8(SECTION_ENTITIES).unwrap();
        frame.count(0).unwrap();
        frame.u8(SECTION_MENUS).unwrap();
        frame.count(0).unwrap();
        assert!(JavaContentManifest::decode(&frame.bytes).is_err());
        let mut frame = Writer::default();
        body(&mut frame);
        frame.count(1).unwrap();
        frame.u8(SECTION_MENUS).unwrap();
        frame.count(1).unwrap();
        frame.count(1).unwrap();
        frame.u8(0xff).unwrap(); // not UTF-8
        assert!(JavaContentManifest::decode(&frame.bytes).is_err());
        // A v2 frame cut before its sections is truncated.
        let mut frame = Writer::default();
        frame.u16(VERSION_V2).unwrap();
        source.encode_body(&mut frame).unwrap();
        assert!(JavaContentManifest::decode(&frame.bytes).is_err());
        assert!(JavaContentManifest::decode(&frame.bytes[..frame.bytes.len() - 1]).is_err());
    }

    /// A v3 frame carries the declaring root's epoch and a hash of the
    /// declarations in its header; the epoch survives the round trip, and a
    /// header hash that does not match the declarations rejects.
    #[test]
    fn v3_header_carries_the_epoch_and_a_content_hash() {
        let mut source = manifest();
        source.epoch = 7;
        let bytes = source.encode().unwrap();
        assert_eq!(u16::from_be_bytes([bytes[0], bytes[1]]), VERSION);
        let (decoded, version) = JavaContentManifest::decode_frame(&bytes).unwrap();
        assert_eq!(version, VERSION);
        assert_eq!(decoded, source);
        assert_eq!(decoded.epoch, 7);
        // Another epoch is the same declarations, so the hash is unchanged
        // and only the epoch bytes differ.
        let mut other_epoch = manifest();
        other_epoch.epoch = 9;
        assert_eq!(
            source.content_hash().unwrap(),
            other_epoch.content_hash().unwrap()
        );
        let other = other_epoch.encode().unwrap();
        assert_ne!(bytes[2..10], other[2..10], "the epoch is in the header");
        assert_eq!(bytes[10..], other[10..], "the declarations are identical");
        // A changed declaration changes the hash.
        let mut other_content = manifest();
        other_content.blocks.push("ruby:other".into());
        assert_ne!(
            source.content_hash().unwrap(),
            other_content.content_hash().unwrap()
        );
        // Flipping a header hash byte rejects before anything is applied.
        let mut wrong_hash = bytes.clone();
        wrong_hash[17] ^= 0xff;
        assert!(JavaContentManifest::decode(&wrong_hash).is_err());
    }

    #[test]
    fn applying_a_manifest_stores_it_for_the_guest() {
        let _guard = lock();
        clear_declared_java_content();
        let manifest = manifest();
        let epoch_before = STORE_EPOCH.load(Ordering::Acquire);
        let count = manifest.apply().unwrap();
        assert!(count > 0);
        assert!(STORE_EPOCH.load(Ordering::Acquire) > epoch_before);
        let content = stored();
        assert_eq!(content.ids.blocks, ["ruby:block"]);
        assert_eq!(content.ids.items, ["ruby:block", "ruby:gem"]);
        assert_eq!(content.ids.menus, ["ruby:crate"]);
        assert_eq!(
            content.block_entities["ruby:crate"].template,
            "minecraft:barrel"
        );
        assert_eq!(content.menus["ruby:crate"].title, "container.ruby.crate");
        assert_eq!(content.menus["ruby:crate"].slots, 27);
        assert_eq!(content.entities["ruby:beast"].template, "minecraft:pig");
        assert_eq!(content.entities["ruby:beast"].category, "creature");
        // The host rebuilds the same v2 sections from the stores.
        let rebuilt = DeclaredContent::from_stored(&content);
        assert_eq!(rebuilt.menu_rows, manifest.menu_rows);
        assert_eq!(rebuilt.entity_rows, manifest.entity_rows);
        // The same manifest again is idempotent.
        assert_eq!(manifest.apply().unwrap(), count);
        // A different non-empty manifest conflicts and leaves the stores.
        let mut other = manifest.clone();
        other.blocks = vec!["ruby:other".into()];
        assert!(other.apply().is_err());
        assert_eq!(stored().ids.blocks, ["ruby:block"]);
    }

    #[test]
    fn applying_a_manifest_over_local_declarations_conflicts() {
        let _guard = lock();
        clear_declared_java_content();
        let local = Registries {
            blocks: vec!["ruby:local".into()],
            ..Registries::default()
        };
        DeclaredContent::normalize(local, Vec::new(), Vec::new(), Vec::new())
            .map(|content| *store() = Some(content))
            .unwrap();
        assert!(manifest().apply().is_err());
        // An empty incoming manifest is not a conflict and changes nothing.
        let empty = JavaContentManifest::default();
        assert_eq!(empty.apply().unwrap(), 1);
        assert_eq!(
            stored().ids.blocks,
            ["ruby:local"],
            "local declarations survive"
        );
    }

    /// A v3 frame whose declarations differ from the non-empty local ones is
    /// refused with a bounded one-line reason: nothing is applied, no epoch is
    /// recorded, and the same frame applies once the local content is gone.
    #[test]
    fn a_v3_manifest_over_different_local_declarations_is_refused() {
        let _guard = lock();
        clear_declared_java_content();
        let local = Registries {
            blocks: vec!["ruby:local".into()],
            ..Registries::default()
        };
        *store() =
            Some(DeclaredContent::normalize(local, Vec::new(), Vec::new(), Vec::new()).unwrap());
        assert_ne!(stored().content_hash(), 0);
        let epoch_before = STORE_EPOCH.load(Ordering::Acquire);
        let applied_before = applied_epoch();

        let mut incoming = manifest();
        incoming.epoch = 12;
        let bytes = incoming.encode().unwrap();
        match apply(&bytes).unwrap() {
            Outcome::Refused(reason) => {
                assert!(reason.contains("keeping the local content"), "{reason}");
                assert!(reason.len() <= MAX_REFUSAL);
                assert!(!reason.contains('\n'));
                assert!(reason.is_ascii());
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(stored().ids.blocks, ["ruby:local"]);
        assert_eq!(STORE_EPOCH.load(Ordering::Acquire), epoch_before);
        assert_eq!(applied_epoch(), applied_before, "a refusal records nothing");

        // With no local declarations the same frame applies and is recorded.
        clear_declared_java_content();
        assert_eq!(apply(&bytes).unwrap(), Outcome::Applied(stored().count()));
        assert_eq!(stored().ids.blocks, ["ruby:block"]);
        assert_eq!(applied_epoch(), 12);
        // Re-sending the same content is the same hash, so not a conflict.
        assert!(matches!(apply(&bytes).unwrap(), Outcome::Applied(_)));
        assert_eq!(applied_epoch(), 12);
    }

    /// An empty v3 frame is not a conflict, as an empty v2 frame never was.
    #[test]
    fn an_empty_v3_frame_over_local_declarations_is_not_refused() {
        let _guard = lock();
        clear_declared_java_content();
        let local = Registries {
            blocks: vec!["ruby:local".into()],
            ..Registries::default()
        };
        *store() =
            Some(DeclaredContent::normalize(local, Vec::new(), Vec::new(), Vec::new()).unwrap());
        let empty = JavaContentManifest::default();
        let outcome = apply(&empty.encode().unwrap()).unwrap();
        assert!(matches!(outcome, Outcome::Applied(_)), "{outcome:?}");
        assert_eq!(stored().ids.blocks, ["ruby:local"]);
    }

    /// A v1 or v2 frame follows the plain merge rule rather than the v3
    /// refusal: a guest with its own declarations gets the merge error, and a
    /// guest without any applies it.
    #[test]
    fn legacy_frames_follow_the_plain_merge_rule() {
        let _guard = lock();
        let source = manifest();
        let mut writer = Writer::default();
        writer.u16(VERSION_V2).unwrap();
        source.encode_body(&mut writer).unwrap();
        source.encode_sections(&mut writer).unwrap();
        let frame = writer.bytes;

        clear_declared_java_content();
        assert!(matches!(apply(&frame).unwrap(), Outcome::Applied(_)));
        assert_eq!(stored().ids.blocks, ["ruby:block"]);

        clear_declared_java_content();
        *store() = Some(
            DeclaredContent::normalize(
                Registries {
                    blocks: vec!["ruby:local".into()],
                    ..Registries::default()
                },
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .unwrap(),
        );
        assert!(apply(&frame).is_err(), "a v2 frame merges plainly");
        assert_eq!(stored().ids.blocks, ["ruby:local"]);
    }

    /// A v1 frame has ids and block-entity rows only; applying it stores them
    /// and leaves the template tables empty, exactly as the old decoder did.
    #[test]
    fn a_v1_frame_applies_without_templates() {
        let _guard = lock();
        let source = manifest();
        let mut writer = Writer::default();
        writer.u16(VERSION_V1).unwrap();
        source.encode_body(&mut writer).unwrap();

        clear_declared_java_content();
        assert!(matches!(apply(&writer.bytes).unwrap(), Outcome::Applied(_)));
        let content = stored();
        assert_eq!(content.ids.blocks, ["ruby:block"]);
        assert_eq!(content.block_entities.len(), 2);
        assert!(content.menus.is_empty());
        assert!(content.entities.is_empty());
    }

    #[test]
    fn rows_keep_their_optional_ticker_flag() {
        let mut source = manifest();
        source.block_entities = vec![
            BlockEntityRow {
                block: "ruby:crate".into(),
                id: "ruby:crate_entity".into(),
                template: "minecraft:barrel".into(),
                ticker: true,
            },
            row("ruby:still", "ruby:still_entity", "minecraft:chest"),
        ];
        let decoded = JavaContentManifest::decode(&source.encode().unwrap()).unwrap();
        let ticker: Vec<bool> = decoded
            .block_entities
            .iter()
            .map(|row| row.ticker)
            .collect();
        assert_eq!(ticker, [true, false]);
    }

    /// The host's snapshot maps every typed declaration table into the frame:
    /// ids, block-entity rows, menu templates and entity flags.
    #[test]
    fn the_registry_snapshot_becomes_the_login_manifest() {
        let declarations = NativeDeclarations {
            blocks: vec![Block {
                id: "ruby:block".into(),
                template: "minecraft:stone".into(),
                item: true,
                ..Block::default()
            }],
            menus: vec![MenuDeclaration {
                id: "ruby:crate".into(),
                slots: 27,
                data_slots: 0,
                results: Vec::new(),
                title_key: Some("container.ruby.crate".into()),
                open_blocks: vec!["ruby:block".into()],
            }],
            block_entities: vec![BlockEntityDeclaration {
                id: "ruby:crate_entity".into(),
                template: "minecraft:barrel".into(),
                ticker: true,
                valid_blocks: vec!["ruby:block".into(), "ruby:box".into()],
            }],
            entities: vec![EntityType {
                id: "ruby:beast".into(),
                template: "minecraft:zombie".into(),
                item: None,
                damage: 0.0,
                attributes: BTreeMap::from([("minecraft:max_health".into(), 40.0)]),
            }],
            ..NativeDeclarations::default()
        };
        let snapshot = JavaRegistrySnapshot::from_declarations(&[declarations]).unwrap();
        let manifest = JavaContentManifest::from_snapshot(&snapshot);
        assert_eq!(manifest.blocks, ["ruby:block"]);
        assert_eq!(manifest.items, ["ruby:block"], "a block's item travels");
        assert_eq!(manifest.menus, ["ruby:crate"]);
        assert_eq!(manifest.block_entity_types, ["ruby:crate_entity"]);
        assert_eq!(manifest.block_entities.len(), 2);
        assert!(
            manifest
                .block_entities
                .iter()
                .all(|row| row.id == "ruby:crate_entity" && row.ticker)
        );
        assert_eq!(manifest.menu_rows[0].title, "container.ruby.crate");
        assert_eq!(manifest.menu_rows[0].slots, 27);
        assert_eq!(manifest.menu_rows[0].open_blocks, ["ruby:block"]);
        assert_eq!(manifest.entity_rows.len(), 1);
        assert_eq!(manifest.entity_rows[0].template, "minecraft:zombie");
        assert_eq!(
            manifest.entity_rows[0].flags,
            ENTITY_FLAG_SPAWN | ENTITY_FLAG_ATTRIBUTES
        );
        // The snapshot manifest is a valid v3 frame.
        let mut manifest = manifest;
        manifest.epoch = 3;
        let (decoded, version) =
            JavaContentManifest::decode_frame(&manifest.encode().unwrap()).unwrap();
        assert_eq!(version, VERSION);
        assert_eq!(decoded, manifest);
    }

    /// A guest needs the manifest once per epoch; a new epoch with identical
    /// content is absorbed, and a new epoch with new content sends.
    #[test]
    fn a_guest_receives_each_epoch_once() {
        assert_eq!(content_action(None, 4, 0x11), ContentSend::Send);
        assert_eq!(content_action(Some((4, 0x11)), 4, 0x11), ContentSend::Skip);
        assert_eq!(
            content_action(Some((4, 0x11)), 5, 0x11),
            ContentSend::Absorb
        );
        assert_eq!(content_action(Some((4, 0x11)), 5, 0x22), ContentSend::Send);
        assert_eq!(content_action(Some((5, 0x22)), 5, 0x22), ContentSend::Skip);
        assert_eq!(
            content_action(Some((5, 0x22)), 6, 0x22),
            ContentSend::Absorb
        );
    }

    /// The host epoch moves only when the declarations' hash moves, so a
    /// content root rebuild with identical rows is not a new snapshot.
    #[test]
    fn the_host_epoch_follows_the_content_hash() {
        let _guard = lock();
        clear_declared_java_content();
        let first = host_manifest();
        assert_eq!(first.epoch, 1, "the first snapshot is epoch 1");
        assert_eq!(
            host_manifest().epoch,
            first.epoch,
            "same content, same epoch"
        );

        *store() = Some(manifest().declared().unwrap());
        let second = host_manifest();
        assert_eq!(second.epoch, first.epoch + 1);
        assert!(!second.is_empty());
        assert_eq!(host_manifest().epoch, second.epoch);
    }
}

//! Repeated saves reuse a block entity's saved tag only while every input to
//! it is unchanged, and write the same bytes as an uncached save.
use super::block_entity_cache::{BlockEntityCache, DEFAULT_BYTES, Inputs, eligible};
use super::section_nbt::tests::{test_biomes, test_registry};
use super::*;
use crate::block_entities::BlockEntities;
use crate::containers::BlockEntity;
use crate::items::ItemStack;
use horizon_core::nbt::Limits;
use horizon_world::region::RegionFile;
use std::time::Duration;
use std::{collections::HashMap, fs};

const NAMES: [&str; 10] = [
    "minecraft:air",
    "minecraft:stone",
    "minecraft:chest",
    "minecraft:trapped_chest",
    "minecraft:furnace",
    "minecraft:oak_sign",
    "minecraft:hopper",
    "minecraft:barrel",
    "minecraft:white_banner",
    "minecraft:skeleton_skull",
];
type Pos = (i32, i32, i32);

struct World {
    registry: &'static BlockRegistry,
    biomes: Arc<Biomes>,
    chunks: Vec<Arc<Chunk>>,
    store: BlockEntities,
    names: HashMap<Pos, Tag>,
}

fn stack(i: usize) -> ItemStack {
    ItemStack {
        item: [
            "minecraft:stone",
            "minecraft:oak_log",
            "minecraft:diamond_sword",
        ][i % 3]
            .into(),
        count: if i % 3 == 2 { 1 } else { 64 },
        damage: 0,
        extra: i
            .is_multiple_of(2)
            .then(|| {
                crate::item_data::decode_components(Some(&Tag::compound([(
                    "minecraft:custom_name",
                    Tag::String(format!("Item {i}").as_str().into()),
                )])))
                .1
            })
            .flatten(),
    }
}

/// Two chunks of chests, trapped chests, furnaces, signs, hoppers, barrels, banners and skulls,
/// some named and some with a loaded source carrying a field Horizon does not model.
fn world() -> World {
    let registry: &'static BlockRegistry = Box::leak(Box::new(test_registry(&NAMES, false)));
    let biomes = test_biomes(&["plains"]);
    let mut store = BlockEntities::default();
    let mut names = HashMap::new();
    let mut chunks = Vec::new();
    for cx in 0..2 {
        let mut chunk = Chunk::empty(cx, 0);
        chunk.lit = true;
        let mut extras = ChunkExtras::default();
        let mut entities = Vec::new();
        for j in 0..28usize {
            let p = (cx * 16 + (j % 16) as i32, 64 + (j / 16) as i32, 3);
            let kind = if j >= 24 { 6 + j % 2 } else { j % 6 };
            let (block, entity) = match kind {
                0 => (5, BlockEntity::for_block(NAMES[5]).unwrap()),
                1 => (4, BlockEntity::for_block(NAMES[4]).unwrap()),
                2 => (6, BlockEntity::for_block(NAMES[6]).unwrap()),
                3 => (
                    7,
                    BlockEntity::Chest((0..27).map(|k| Some(stack(j + k))).collect()),
                ),
                4 => (3, BlockEntity::Chest(vec![Some(stack(j)); 27])),
                5 => (
                    2,
                    BlockEntity::Chest((0..27).map(|k| Some(stack(k))).collect()),
                ),
                6 => (8, BlockEntity::for_block(NAMES[8]).unwrap()),
                _ => (9, BlockEntity::for_block(NAMES[9]).unwrap()),
            };
            chunk.set(p.0 & 15, p.1, p.2 & 15, block);
            if j % 4 == 3 {
                names.insert(p, Tag::String(format!("Box {j}").as_str().into()));
            }
            if j % 3 == 0 {
                let mut source = block_entity_encode::encode(p, &entity, NAMES[block as usize]);
                if let Tag::Compound(map) = &mut source {
                    map.insert("horizon:test".into(), Tag::Int(j as i32));
                }
                extras.block_entity_sources.push((p, source));
            }
            extras.block_entity_order.push(p);
            entities.push((p, entity));
        }
        store.load_chunk(entities);
        chunk.saved = Some(Arc::new(extras));
        chunks.push(Arc::new(chunk));
    }
    World {
        registry,
        biomes,
        chunks,
        store,
        names,
    }
}

impl World {
    fn snapshot(&self) -> Vec<Arc<Chunk>> {
        chunks_with_revised_block_entities(&self.chunks, &self.store, &self.names)
            .into_iter()
            .map(Arc::new)
            .collect()
    }
    fn cached(&self, chunk: &Chunk, cache: &BlockEntityCache) -> Vec<u8> {
        let caches = SaveCaches {
            palettes: None,
            block_entities: Some(cache),
        };
        let root = encode_saved(
            chunk,
            self.registry,
            &self.biomes,
            None,
            LevelShape::OVERWORLD,
            None,
            caches,
        );
        nbt::write_named(&root, Limits::saved()).unwrap()
    }
    fn fresh(&self, chunk: &Chunk) -> Vec<u8> {
        let root = encode_with(
            chunk,
            self.registry,
            &self.biomes,
            None,
            LevelShape::OVERWORLD,
        );
        nbt::write_named(&root, Limits::saved()).unwrap()
    }
    /// Saves a fresh snapshot with `cache`, checks it against an uncached
    /// encode, and returns how many block entities hit.
    fn save_round(&self, cache: &BlockEntityCache) -> usize {
        let before = cache.hits();
        for chunk in self.snapshot() {
            assert_eq!(self.cached(&chunk, cache), self.fresh(&chunk));
        }
        cache.hits() - before
    }
    /// The two saves before reuse: the first notes revisions, the second
    /// keeps the tags of the entities that stayed the same.
    fn warm(&self, cache: &BlockEntityCache) {
        assert_eq!(self.save_round(cache), 0);
        assert_eq!(self.save_round(cache), 0);
    }
    fn entity_count(&self) -> usize {
        self.store.len()
    }
}

#[test]
fn repeated_saves_hit_and_write_the_same_nbt() {
    let w = world();
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    assert_eq!(cache.len(), 0, "nothing is cached before a save");
    assert_eq!(w.save_round(&cache), 0);
    assert_eq!(cache.len(), 0, "the first save notes revisions alone");
    assert_eq!(w.save_round(&cache), 0);
    assert_eq!(cache.len(), w.entity_count(), "the second keeps the tags");
    assert_eq!(w.save_round(&cache), w.entity_count());
    assert_eq!(w.save_round(&cache), w.entity_count());
}

#[test]
fn a_mutated_block_entity_saves_its_new_contents() {
    let mut w = world();
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    w.warm(&cache);
    let pos = (3, 64, 3);
    let old = w.fresh(&w.snapshot()[0]);
    match w.store.get_mut(&pos) {
        Some(BlockEntity::Chest(items)) => items[0].as_mut().unwrap().count = 5,
        other => panic!("chest expected, got {other:?}"),
    }
    assert_eq!(w.save_round(&cache), w.entity_count() - 1);
    assert_ne!(w.fresh(&w.snapshot()[0]), old);
    // Each other mutable path misses for what it lent.
    w.store.iter_mut().for_each(drop);
    assert_eq!(w.save_round(&cache), 0);
    w.store.values_mut().for_each(drop);
    assert_eq!(w.save_round(&cache), 0);
    w.store
        .insert(pos, BlockEntity::Chest(vec![Some(stack(1)); 27]));
    let (lease, entity) = w.store.take(&(4, 64, 3)).unwrap();
    assert!(w.store.put(lease, entity));
    w.store.retain(|p, _| *p != (5, 64, 3));
    assert_eq!(w.save_round(&cache), 0, "retain lent every kept entity");
    w.store.remove(&(6, 64, 3));
    w.store
        .get_or_insert_with((6, 64, 3), || BlockEntity::for_block(NAMES[5]).unwrap());
    // Changed on every save so far, they are noted by revision alone: a quiet
    // save serializes them again, and the next one reuses them.
    assert_eq!(cache.len(), 0);
    assert_eq!(w.save_round(&cache), 0);
    assert_eq!(cache.len(), w.entity_count() - 1, "all but the new one");
    assert_eq!(w.save_round(&cache), w.entity_count() - 1);
    assert_eq!(w.save_round(&cache), w.entity_count());
}

#[test]
fn an_entity_changed_every_save_is_not_serialized_again() {
    let mut w = world();
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    let busy = (3, 64, 3);
    let count = w.entity_count();
    for round in 0..6 {
        let hits = w.save_round(&cache);
        assert_eq!(hits, if round < 2 { 0 } else { count - 1 });
        if round >= 1 {
            assert_eq!(cache.len(), count - 1, "only its revision is noted");
        }
        w.store.get_mut(&busy);
    }
    // Left alone: the next save notes its revision, the one after keeps its
    // tag, and the third reuses it.
    assert_eq!(w.save_round(&cache), count - 1);
    assert_eq!(cache.len(), count - 1);
    assert_eq!(w.save_round(&cache), count - 1);
    assert_eq!(cache.len(), count);
    assert_eq!(w.save_round(&cache), count);
}

#[test]
fn a_full_cache_keeps_what_it_reuses() {
    let w = world();
    let cache = BlockEntityCache::new(16 * 1024);
    w.warm(&cache);
    let kept = cache.len();
    let hits: Vec<usize> = (0..4).map(|_| w.save_round(&cache)).collect();
    assert!(kept > 0 && hits[0] > 0);
    assert!(hits.iter().all(|h| *h == hits[0]), "{hits:?} stay stable");
    assert_eq!(cache.len(), kept);
}

#[test]
fn a_full_cache_evicts_only_tags_unused_for_a_while() {
    let chest = Tag::compound([("id", Tag::String("minecraft:chest".into()))]);
    let inputs = Inputs {
        block: "minecraft:chest",
        name: None,
        source: None,
    };
    // Two saves of the same revision keep a tag; a third reuses it.
    let keep = |cache: &BlockEntityCache, positions: std::ops::Range<i32>| {
        for x in positions.clone() {
            cache.put((x, 0, 0), x as u64 + 1, &inputs, chest.clone());
            cache.put((x, 0, 0), x as u64 + 1, &inputs, chest.clone());
        }
        positions
            .filter(|x| cache.get((*x, 0, 0), *x as u64 + 1, &inputs).is_some())
            .count()
    };
    for (protect, new_kept) in [(Duration::from_secs(3600), false), (Duration::ZERO, true)] {
        let cache = BlockEntityCache::protecting(16 * 1024, protect);
        let old = keep(&cache, 0..1000);
        assert!(old > 0 && old < 1000, "{old} kept of 1000");
        let full = cache.len();
        let new = keep(&cache, 1000..2000);
        assert_eq!(new > 0, new_kept, "{new} new tags kept");
        assert!(cache.bytes() <= 16 * 1024);
        assert!(cache.len() <= full + 16);
    }
}

#[test]
fn the_other_inputs_are_compared_exactly() {
    let mut w = world();
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    w.warm(&cache);
    let all = w.entity_count();
    // The block at a chest becomes a trapped chest: same entity, other id.
    let chest = (5, 64, 3);
    Arc::make_mut(&mut w.chunks[0]).set(chest.0, chest.1, chest.2, 3);
    assert_eq!(w.save_round(&cache), all - 1);
    // A name is added, changed and removed.
    w.names.insert(chest, Tag::String("Renamed".into()));
    assert_eq!(w.save_round(&cache), all - 1);
    w.names.insert(chest, Tag::String("Again".into()));
    assert_eq!(w.save_round(&cache), all - 1);
    w.names.remove(&chest);
    assert_eq!(w.save_round(&cache), all - 1);
    // The loaded source changes (a hopper's TransferCooldown is written into it on save).
    let sourced = (0, 64, 3);
    let chunk = Arc::make_mut(&mut w.chunks[0]);
    let extras = Arc::make_mut(chunk.saved.as_mut().unwrap());
    let (_, source) = extras
        .block_entity_sources
        .iter_mut()
        .find(|(p, _)| *p == sourced)
        .unwrap();
    if let Tag::Compound(map) = source {
        map.insert("horizon:test".into(), Tag::Int(-1));
    }
    assert_eq!(w.save_round(&cache), all - 1);
    let chunk = Arc::make_mut(&mut w.chunks[0]);
    let extras = Arc::make_mut(chunk.saved.as_mut().unwrap());
    extras.block_entity_sources.retain(|(p, _)| *p != sourced);
    assert_eq!(w.save_round(&cache), all - 1);
}

#[test]
fn per_save_overlays_apply_after_a_hit() {
    let mut w = world();
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    w.warm(&cache);
    let chest = (5, 64, 3);
    let chunk = Arc::make_mut(&mut w.chunks[0]);
    let extras = Arc::make_mut(chunk.saved.as_mut().unwrap());
    extras.pending_loot.insert(
        chest,
        crate::chunk::PendingContainerLoot {
            table: Tag::String("minecraft:chests/simple_dungeon".into()),
            seed: Some(Tag::Long(7)),
        },
    );
    if let Some((_, Tag::Compound(source))) = extras
        .block_entity_sources
        .iter_mut()
        .find(|(p, _)| *p == (0, 64, 3))
    {
        source.insert("horizon:stack_limit".into(), Tag::Int(16));
    }
    // The chest still hits, and its save has the loot table; the sign's changed source misses.
    assert_eq!(w.save_round(&cache), w.entity_count() - 1);
    let saved = w.fresh(&w.snapshot()[0]);
    let (root, _) = nbt::read_named(&saved, Limits::saved()).unwrap();
    let Some(Tag::List(list)) = root.tag.get("block_entities") else {
        panic!("block entities")
    };
    let entry = list
        .iter()
        .find(|t| number(t.get("x")) == Some(5) && number(t.get("y")) == Some(64))
        .unwrap();
    assert!(entry.get("LootTable").is_some());
}

#[test]
fn snapshots_without_revisions_never_hit() {
    let w = world();
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    for _ in 0..2 {
        for chunk in chunks_with_block_entities(&w.chunks, &w.store, &w.names) {
            assert_eq!(w.cached(&chunk, &cache), w.fresh(&chunk));
        }
    }
    assert_eq!((cache.len(), cache.hits()), (0, 0));
    // A snapshot whose list was replaced after it was made has no revisions.
    let mut chunk = (*w.snapshot()[0]).clone();
    chunk.block_entities.reverse();
    assert_eq!(chunk.block_entity_revision(0), None);
    chunk.block_entities.pop();
    assert_eq!(chunk.block_entity_revision(0), None);
}

#[test]
fn mod_block_entities_are_not_eligible() {
    let fields = crate::chunk::block_entity::ModBlockEntityFields {
        nbt: Default::default(),
        items: Vec::new(),
        data: Vec::new(),
    };
    let entity = crate::chunk::block_entity::ModBlockEntity::new("demo:box".into(), fields);
    assert!(!eligible(&BlockEntity::Mod(Box::new(entity))));
    assert!(eligible(&BlockEntity::Chest(Vec::new())));
    assert!(eligible(&BlockEntity::for_block(NAMES[8]).unwrap()));
    assert!(!eligible(
        &BlockEntity::for_block("minecraft:beehive").unwrap()
    ));
}

#[test]
fn the_budget_caps_what_is_kept() {
    let w = world();
    // 16 shards of 1 KiB: a sign, furnace or hopper fits, a chest of 27 stacks does not.
    let cache = BlockEntityCache::new(16 * 1024);
    w.warm(&cache);
    let hits = w.save_round(&cache);
    assert!(cache.len() > 0 && cache.len() < w.entity_count());
    assert!(hits > 0 && hits <= cache.len());
    assert!(cache.bytes() <= 16 * 1024);
    let none = BlockEntityCache::new(0);
    w.warm(&none);
    assert_eq!(w.save_round(&none), 0);
    assert_eq!(none.len(), 0);
}

#[test]
fn an_entry_over_64_kib_is_not_kept() {
    let mut w = world();
    let pos = (3, 64, 3);
    let big = Tag::String("x".repeat(40 * 1024).as_str().into());
    let item = ItemStack {
        extra: crate::item_data::decode_components(Some(&Tag::compound([(
            "minecraft:custom_data",
            Tag::compound([("blob", big)]),
        )])))
        .1,
        ..stack(0)
    };
    w.store.insert(pos, BlockEntity::Chest(vec![Some(item); 2]));
    let cache = BlockEntityCache::new(DEFAULT_BYTES);
    w.warm(&cache);
    assert_eq!(cache.len(), w.entity_count() - 1);
    assert_eq!(w.save_round(&cache), w.entity_count() - 1);
    assert_eq!(
        cache.len(),
        w.entity_count() - 1,
        "not tried again at its revision"
    );
}

#[test]
fn cached_saves_write_byte_identical_region_files() {
    let mut w = world();
    let write_region =
        |path: &std::path::Path, chunks: &[Arc<Chunk>], cache: Option<Arc<BlockEntityCache>>| {
            let encoding = Arc::new(Encoding {
                registry: w.registry,
                biomes: Arc::clone(&w.biomes),
                tables: None,
                shape: LevelShape::OVERWORLD,
                section_palettes: None,
                block_entities: cache,
            });
            let records: Vec<((i32, i32), Record)> = chunks
                .iter()
                .map(|c| {
                    let record = Record::Chunk(Arc::clone(c), Arc::clone(&encoding), None);
                    ((c.x, c.z), record)
                })
                .collect();
            let refs: Vec<((i32, i32), &Record)> = records.iter().map(|(k, r)| (*k, r)).collect();
            let compressed = compress_records(&refs, 1_700_000_000);
            fs::create_dir_all(path).unwrap();
            let file = path.join("r.0.0.mca");
            let mut region = RegionFile::open_writable(&file, true).unwrap();
            for ((cx, cz), record) in records.iter().map(|(k, _)| *k).zip(compressed) {
                let record = record.unwrap().expect("chunk record");
                region.write(ChunkPos::new(cx, cz), &record).unwrap();
            }
            region.sync().unwrap();
            fs::read(&file).unwrap()
        };
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(BlockEntityCache::new(DEFAULT_BYTES));
    for round in 0..4 {
        if round == 3 {
            if let Some(BlockEntity::Chest(items)) = w.store.get_mut(&(9, 64, 3)) {
                items[3] = None;
            }
            w.names.insert((17, 64, 3), Tag::String("Late".into()));
        }
        let chunks = w.snapshot();
        let uncached = write_region(&dir.path().join(format!("u{round}")), &chunks, None);
        let cached = write_region(
            &dir.path().join(format!("c{round}")),
            &chunks,
            Some(Arc::clone(&cache)),
        );
        assert_eq!(cached, uncached, "round {round}");
    }
    assert!(cache.hits() >= w.entity_count());
}

#[test]
fn a_save_dir_fills_on_save_not_on_load() {
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let save = SaveDir::new(dir.path().to_path_buf(), LevelShape::OVERWORLD);
    save.save(&w.snapshot(), w.registry, &w.biomes).unwrap();
    assert_eq!(save.block_entities.len(), 0);
    save.save(&w.snapshot(), w.registry, &w.biomes).unwrap();
    assert_eq!(save.block_entities.len(), w.entity_count());
    let reloaded = SaveDir::new(dir.path().to_path_buf(), LevelShape::OVERWORLD);
    let loaded = reloaded.load(0, 0, w.registry, &w.biomes).expect("reloads");
    assert_eq!(reloaded.block_entities.len(), 0);
    assert_eq!(loaded.block_entities.len(), 28);
    save.save(&w.snapshot(), w.registry, &w.biomes).unwrap();
    assert_eq!(save.block_entities.hits(), w.entity_count());
    // The queued path shares the folder's cache.
    save.save_later(&w.snapshot(), w.registry, &w.biomes)
        .unwrap();
    save.flush().unwrap();
    assert_eq!(save.block_entities.hits(), 2 * w.entity_count());
}

/// A storage area: 64 chunks of 128 block entities each, every one with the
/// source it was loaded with. Run with `cargo test --release -p horizon-client-lib
/// --lib bench_repeated_block_entity_saves -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not a check"]
fn bench_repeated_block_entity_saves() {
    use std::time::Instant;
    let registry: &'static BlockRegistry = Box::leak(Box::new(test_registry(&NAMES, false)));
    let biomes = test_biomes(&["plains"]);
    let mut store = BlockEntities::default();
    let names = HashMap::new();
    let mut chunks = Vec::new();
    for c in 0..64 {
        let (cx, cz) = (c % 8, c / 8);
        let mut chunk = Chunk::empty(cx, cz);
        chunk.lit = true;
        let mut extras = ChunkExtras::default();
        let mut entities = Vec::new();
        for j in 0..128usize {
            let p = (
                cx * 16 + (j % 16) as i32,
                64 + (j / 64) as i32,
                cz * 16 + (j / 16 % 4) as i32,
            );
            let (block, mut entity) = match j % 20 {
                0..=3 => (5, BlockEntity::for_block(NAMES[5]).unwrap()),
                4..=6 => (4, BlockEntity::for_block(NAMES[4]).unwrap()),
                7..=9 => (
                    6,
                    BlockEntity::Chest((0..5).map(|k| Some(stack(j + k))).collect()),
                ),
                10..=14 => (
                    7,
                    BlockEntity::Chest((0..27).map(|k| Some(stack(j + k))).collect()),
                ),
                _ => (2, BlockEntity::Chest(vec![Some(stack(j)); 27])),
            };
            match &mut entity {
                BlockEntity::Furnace(f) => {
                    f.items[0] = Some(stack(j));
                    f.lit_remaining = 80;
                }
                BlockEntity::Sign(s) => s.front.lines[0] = format!("Row {j}").as_str().into(),
                _ => {}
            }
            chunk.set(p.0 & 15, p.1, p.2 & 15, block);
            let source = block_entity_encode::encode(p, &entity, NAMES[block as usize]);
            extras.block_entity_sources.push((p, source));
            extras.block_entity_order.push(p);
            entities.push((p, entity));
        }
        store.load_chunk(entities);
        chunk.saved = Some(Arc::new(extras));
        chunks.push(Arc::new(chunk));
    }
    let snapshot = |chunks: &[Arc<Chunk>], store: &BlockEntities| -> Vec<Arc<Chunk>> {
        chunks_with_revised_block_entities(chunks, store, &names)
            .into_iter()
            .map(Arc::new)
            .collect()
    };
    let encode = |chunks: &[Arc<Chunk>], cache: Option<&BlockEntityCache>| -> Vec<NamedTag> {
        let caches = SaveCaches {
            palettes: None,
            block_entities: cache,
        };
        let shape = LevelShape::OVERWORLD;
        chunks
            .iter()
            .map(|c| encode_saved(c, registry, &biomes, None, shape, None, caches))
            .collect()
    };
    let full = |chunks: &[Arc<Chunk>], cache: Option<Arc<BlockEntityCache>>| -> Vec<Vec<u8>> {
        let encoding = Arc::new(Encoding {
            registry,
            biomes: Arc::clone(&biomes),
            tables: None,
            shape: LevelShape::OVERWORLD,
            section_palettes: None,
            block_entities: cache,
        });
        let records: Vec<_> = chunks
            .iter()
            .map(|c| {
                (
                    (c.x, c.z),
                    Record::Chunk(Arc::clone(c), Arc::clone(&encoding), None),
                )
            })
            .collect();
        let refs: Vec<_> = records.iter().map(|(k, r)| (*k, r)).collect();
        compress_records(&refs, 1_700_000_000)
            .into_iter()
            .map(|r| r.unwrap().unwrap().compressed)
            .collect()
    };
    let best = |mut f: Box<dyn FnMut() + '_>| {
        (0..7)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .fold(f64::INFINITY, f64::min)
    };
    // Loaded from a world this encoder wrote: each source is what it saves.
    if std::env::var_os("BE_RAW_SOURCES").is_none() {
        let first = encode(&snapshot(&chunks, &store), None);
        for (chunk, saved) in chunks.iter_mut().zip(first) {
            let Some(Tag::List(list)) = saved.tag.get("block_entities") else {
                panic!("block entities")
            };
            let extras = Arc::make_mut(Arc::make_mut(chunk).saved.as_mut().unwrap());
            extras.block_entity_sources = list
                .iter()
                .map(|t| {
                    let at = |k| number(t.get(k)).unwrap() as i32;
                    ((at("x"), at("y"), at("z")), t.clone())
                })
                .collect();
        }
    }
    let chunks_now = snapshot(&chunks, &store);
    let expected = encode(&chunks_now, None);
    let uncached = best(Box::new(|| drop(encode(&chunks_now, None))));
    let cold = best(Box::new(|| {
        let cache = BlockEntityCache::new(DEFAULT_BYTES);
        drop(encode(&chunks_now, Some(&cache)));
    }));
    let mut fill = f64::INFINITY;
    for _ in 0..7 {
        let cache = BlockEntityCache::new(DEFAULT_BYTES);
        drop(encode(&chunks_now, Some(&cache)));
        let t = Instant::now();
        drop(encode(&chunks_now, Some(&cache)));
        fill = fill.min(t.elapsed().as_secs_f64() * 1e3);
    }
    let budget = std::env::var("BE_BUDGET_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(DEFAULT_BYTES, |m| m << 20);
    let cache = BlockEntityCache::new(budget);
    assert_eq!(encode(&chunks_now, Some(&cache)), expected);
    assert_eq!(encode(&chunks_now, Some(&cache)), expected);
    let warm = best(Box::new(|| drop(encode(&chunks_now, Some(&cache)))));
    // One in ten block entities lent mutably between saves.
    let positions: Vec<Pos> = store.keys().copied().step_by(10).collect();
    for p in &positions {
        store.get_mut(p);
    }
    let touched = snapshot(&chunks, &store);
    let expected_touched = encode(&touched, None);
    let mut tenth = f64::INFINITY;
    for _ in 0..7 {
        for p in &positions {
            store.get_mut(p);
        }
        let touched = snapshot(&chunks, &store);
        let t = Instant::now();
        let out = encode(&touched, Some(&cache));
        let same = out == expected_touched;
        drop(out);
        tenth = tenth.min(t.elapsed().as_secs_f64() * 1e3);
        assert!(same);
    }
    // Every block entity lent mutably between saves: none can be reused.
    let all: Vec<Pos> = store.keys().copied().collect();
    let mut every = f64::INFINITY;
    let mut every_uncached = f64::INFINITY;
    for round in 0..7 {
        for p in &all {
            store.get_mut(p);
        }
        let touched = snapshot(&chunks, &store);
        let t = Instant::now();
        drop(encode(
            &touched,
            if round % 2 == 0 { Some(&cache) } else { None },
        ));
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if round % 2 == 0 {
            every = every.min(ms);
        } else {
            every_uncached = every_uncached.min(ms);
        }
    }
    let chunks_now = snapshot(&chunks, &store);
    let expected = encode(&chunks_now, None);
    let shared = Arc::new(BlockEntityCache::new(budget));
    let bytes = full(&chunks_now, None);
    let full_cold = best(Box::new(|| {
        drop(full(
            &chunks_now,
            Some(Arc::new(BlockEntityCache::new(budget))),
        ));
    }));
    let mut full_fill = f64::INFINITY;
    for _ in 0..7 {
        let fresh = Arc::new(BlockEntityCache::new(budget));
        drop(full(&chunks_now, Some(Arc::clone(&fresh))));
        let t = Instant::now();
        drop(full(&chunks_now, Some(fresh)));
        full_fill = full_fill.min(t.elapsed().as_secs_f64() * 1e3);
    }
    assert_eq!(full(&chunks_now, Some(Arc::clone(&shared))), bytes);
    let full_uncached = best(Box::new(|| drop(full(&chunks_now, None))));
    assert_eq!(full(&chunks_now, Some(Arc::clone(&shared))), bytes);
    let full_warm = best(Box::new(|| {
        drop(full(&chunks_now, Some(Arc::clone(&shared))))
    }));
    assert_eq!(full(&chunks_now, Some(Arc::clone(&shared))), bytes);
    assert_eq!(encode(&chunks_now, Some(&cache)), expected);
    println!(
        "{} block entities in {} chunks, {} compressed bytes",
        store.len(),
        chunks.len(),
        bytes.iter().map(Vec::len).sum::<usize>()
    );
    println!(
        "chunk NBT encode: uncached {uncached:.1} ms, first save {cold:.1} ms, second save {fill:.1} ms, repeat {warm:.1} ms, repeat with 1/10 changed {tenth:.1} ms, all changed {every:.1} ms (uncached {every_uncached:.1} ms)"
    );
    println!(
        "encode + NBT write + zlib (worker pool): uncached {full_uncached:.1} ms, first save {full_cold:.1} ms, second save {full_fill:.1} ms, repeat {full_warm:.1} ms"
    );
    println!(
        "cache: {} entries, {} estimated bytes",
        cache.len(),
        cache.bytes()
    );
}

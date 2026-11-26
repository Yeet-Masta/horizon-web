use super::*;
use std::sync::atomic::AtomicUsize;

fn registry() -> &'static BlockRegistry {
    static REGISTRY: OnceLock<BlockRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let facts: &[u8] = br#"{
                "defaults": {"minecraft:air": 0, "minecraft:bell": 1},
                "states": [
                    ["minecraft:air", true, false, "air", "minecraft:air"],
                    ["minecraft:bell", false, false, "bell", "minecraft:bell"]
                ]
            }"#;
        BlockRegistry::from_facts_json(facts).unwrap()
    })
}

fn biomes() -> &'static Arc<Biomes> {
    static BIOMES: OnceLock<Arc<Biomes>> = OnceLock::new();
    BIOMES.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data/minecraft/worldgen/biome");
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("plains.json"),
            br#"{"effects":{"water_color":0}}"#,
        )
        .unwrap();
        let resources =
            horizon_assets::resource_source::ResourceRoot::directory(root.path().to_path_buf());
        Arc::new(Biomes::load_for_server(&resources).unwrap())
    })
}

fn tables() -> SaveTables {
    SaveTables {
        block_names: vec!["minecraft:air".into(), "minecraft:bell".into()],
        block_ids: HashMap::from([("minecraft:air".into(), 0), ("minecraft:bell".into(), 1)]),
        default_state: vec![true, true],
        block_entity: vec![false, true],
        heightmaps: crate::assets::Properties::for_tests(&[
            (true, false, false, false),
            (false, false, true, true),
        ]),
        air: 0,
        poi: vec![None, Some(("minecraft:meeting", 32))],
        component_registry: None,
        component_items: HashSet::new(),
        component_item_tags: Default::default(),
    }
}

fn assert_same_chunk(left: &Chunk, right: &Chunk, shape: LevelShape) {
    assert_eq!((left.x, left.z), (right.x, right.z));
    for y in shape.min_y..shape.min_y + shape.height {
        for z in 0..16 {
            for x in 0..16 {
                assert_eq!(left.get(x, y, z), right.get(x, y, z), "block {x} {y} {z}");
            }
        }
    }
    assert_eq!(left.structure_refs, right.structure_refs);
    match (left.saved.as_deref(), right.saved.as_deref()) {
        (Some(left), Some(right)) => {
            assert_eq!(left.last_update, right.last_update);
            assert_eq!(left.root, right.root);
            assert_eq!(left.block_entities, right.block_entities);
            assert_eq!(left.block_ticks, right.block_ticks);
            assert_eq!(left.fluid_ticks, right.fluid_ticks);
        }
        (None, None) => {}
        _ => panic!("saved chunk data differs"),
    }
}

#[test]
fn rest_codec_reconstructs_the_non_section_chunk_data() {
    let registry = registry();
    let biomes = biomes();
    let tables = tables();
    let shape = LevelShape::OVERWORLD;
    let mut chunk = Chunk::empty(3, -2);
    chunk.set(1, 70, 2, 1);
    chunk
        .structure_refs
        .push(("minecraft:village".into(), (12, -8)));
    let mut extras = ChunkExtras {
        last_update: 91,
        ..ChunkExtras::default()
    };
    extras.root.insert("example:kept".into(), Tag::Long(42));
    extras.block_entities.push(Tag::compound([
        ("id", string("example:unknown_block_entity")),
        ("x", Tag::Int(49)),
        ("y", Tag::Int(70)),
        ("z", Tag::Int(-30)),
        ("custom", string("kept")),
    ]));
    extras.block_ticks.push(SavedTick {
        kind: TickTypeId(1),
        pos: BlockPos::new(49, 70, -30),
        delay: 17,
        priority: TickPriority::High,
    });
    extras.fluid_ticks.push(SavedTick {
        kind: TickTypeId(fluid_id("minecraft:water").unwrap()),
        pos: BlockPos::new(49, 70, -30),
        delay: 21,
        priority: TickPriority::High,
    });
    chunk.saved = Some(Arc::new(extras));

    let encoded = encode_with(&chunk, registry, biomes, Some(&tables), shape);
    let rest = encode_rest(&chunk, registry, biomes, Some(&tables), shape);
    assert_eq!(rest.tag.get("sections"), Some(&Tag::List(Vec::new())));
    assert!(rest.tag.get("Heightmaps").is_none());
    assert!(rest.tag.get("block_ticks").is_none());
    assert!(rest.tag.get("fluid_ticks").is_none());
    assert_eq!(
        encode_rest_saved(&chunk, registry, biomes, Some(&tables), shape, None).tag,
        rest.tag
    );

    let sections = NamedTag {
        name: encoded.name.clone(),
        tag: Tag::compound([
            (
                "DataVersion",
                encoded.tag.get("DataVersion").unwrap().clone(),
            ),
            ("xPos", Tag::Int(chunk.x)),
            ("zPos", Tag::Int(chunk.z)),
            ("Status", string("minecraft:full")),
            ("sections", encoded.tag.get("sections").unwrap().clone()),
        ]),
    };
    let mut restored = decode_with(&sections, registry, biomes, Some(&tables), shape.column())
        .expect("sections decode");
    let mut rest = rest;
    // A backend stores tick lists separately and puts them back before decode_rest.
    for key in ["block_ticks", "fluid_ticks"] {
        if let Some(ticks) = encoded.tag.get(key) {
            put(&mut rest.tag, key, ticks.clone());
        }
    }
    decode_rest(&rest, &mut restored, registry, Some(&tables));
    let expected = decode_with(&encoded, registry, biomes, Some(&tables), shape.column())
        .expect("full record decodes");
    let saved = expected.saved.as_ref().unwrap();
    assert_eq!(saved.block_entities.len(), 1);
    assert_eq!(saved.block_ticks.len(), 1);
    assert_eq!(saved.fluid_ticks.len(), 1);
    assert_same_chunk(&expected, &restored, shape);
}

type ChunkTags = HashMap<(i32, i32), NamedTag>;
type WriteGate = (mpsc::Sender<()>, mpsc::Receiver<()>);

#[derive(Default)]
struct MemoryBackend {
    records: Mutex<HashMap<String, ChunkTags>>,
    chunks: Mutex<HashMap<(i32, i32), Chunk>>,
    chunk_tags: Mutex<ChunkTags>,
    write_gate: Mutex<Option<WriteGate>>,
    region_chunk_queries: AtomicUsize,
}

impl MemoryBackend {
    fn seed_tag(&self, folder: &str, key: (i32, i32), tag: NamedTag) {
        self.records
            .lock()
            .unwrap()
            .entry(folder.to_owned())
            .or_default()
            .insert(key, tag);
    }
}

impl Backend for MemoryBackend {
    fn load_chunk(
        &self,
        x: i32,
        z: i32,
        _context: &BackendContext<'_>,
    ) -> Result<Option<BackendChunk>> {
        let chunk = self.chunks.lock().unwrap().get(&(x, z)).cloned();
        if let Some(chunk) = chunk {
            return Ok(Some(BackendChunk::Chunk(Box::new(chunk))));
        }
        if let Some(tag) = self.chunk_tags.lock().unwrap().get(&(x, z)).cloned() {
            return Ok(Some(BackendChunk::Tag(tag)));
        }
        Ok(self.read_tag("region", x, z)?.map(BackendChunk::Tag))
    }

    fn read_tag(&self, folder: &str, x: i32, z: i32) -> Result<Option<NamedTag>> {
        Ok(self
            .records
            .lock()
            .map_err(|_| "memory records poisoned")?
            .get(folder)
            .and_then(|records| records.get(&(x, z)))
            .cloned())
    }

    fn write(&self, folder: &str, records: &[((i32, i32), BackendRecord<'_>)]) -> Result<()> {
        let gate = self.write_gate.lock().unwrap().take();
        if let Some((started, release)) = gate {
            started.send(())?;
            release.recv_timeout(horizon_std::time::Duration::from_secs(5))?;
        }
        let mut tags = self.records.lock().map_err(|_| "memory records poisoned")?;
        let mut chunks = self.chunks.lock().map_err(|_| "memory chunks poisoned")?;
        let folder_tags = tags.entry(folder.to_owned()).or_default();
        for (key, record) in records {
            match record {
                BackendRecord::Chunk(chunk, context, blending) => {
                    folder_tags.insert(
                        *key,
                        encode_rest_saved(
                            chunk,
                            context.registry,
                            context.biomes,
                            context.tables,
                            context.shape,
                            *blending,
                        ),
                    );
                    chunks.insert(*key, (**chunk).clone());
                }
                BackendRecord::Tag(tag) => {
                    folder_tags.insert(*key, (*tag).clone());
                    if folder == "region" {
                        chunks.remove(key);
                    }
                }
                BackendRecord::Delete => {
                    folder_tags.remove(key);
                    if folder == "region" {
                        chunks.remove(key);
                    }
                }
            }
        }
        Ok(())
    }

    fn region_chunks(&self, rx: i32, rz: i32) -> Result<Vec<(i32, i32)>> {
        self.region_chunk_queries.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .records
            .lock()
            .map_err(|_| "memory records poisoned")?
            .get("region")
            .into_iter()
            .flat_map(|records| records.keys().copied())
            .filter(|(x, z)| x >> 5 == rx && z >> 5 == rz)
            .collect())
    }
}

#[test]
fn save_dir_routes_chunks_entities_and_poi_through_the_backend() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let save = SaveDir::with_backend(
        root.path().to_path_buf(),
        LevelShape::OVERWORLD,
        backend.clone(),
    );
    assert!(save.tables.set(Arc::new(tables())).is_ok());
    let old_poi = NamedTag {
        name: "".into(),
        tag: Tag::compound([
            ("DataVersion", Tag::Int(DATA_VERSION)),
            (
                "Sections",
                Tag::compound([(
                    "4",
                    Tag::compound([(
                        "Records",
                        Tag::List(vec![Tag::compound([
                            ("pos", Tag::IntArray(vec![1, 70, 2])),
                            ("type", string("minecraft:meeting")),
                            ("free_tickets", Tag::Int(7)),
                        ])]),
                    )]),
                )]),
            ),
        ]),
    };
    backend.seed_tag("poi", (0, 0), old_poi);

    let mut chunk = Chunk::empty(0, 0);
    chunk.set(1, 70, 2, 1);
    chunk.set(2, 70, 2, 1);
    chunk
        .structure_refs
        .push(("minecraft:village".into(), (4, 5)));
    let biomes = biomes();
    save.save_later(&[Arc::new(chunk.clone())], registry(), biomes)
        .unwrap();
    save.flush().unwrap();
    let loaded = save
        .load(0, 0, registry(), biomes)
        .expect("backend chunk loads");
    assert_same_chunk(&loaded, &chunk, LevelShape::OVERWORLD);
    assert!(backend.chunks.lock().unwrap().contains_key(&(0, 0)));

    let entities = vec![Tag::compound([("id", string("example:entity"))])];
    save.save_entities(vec![((0, 0), entities.clone())])
        .unwrap();
    assert_eq!(save.load_entities(0, 0), entities);
    save.save_entities(vec![((0, 0), Vec::new())]).unwrap();
    assert!(backend.read_tag("entities", 0, 0).unwrap().is_none());
    let poi = save.read_record("poi", 0, 0).expect("POI saved");
    let Some(Tag::List(records)) = poi
        .tag
        .get("Sections")
        .and_then(|s| s.get("4"))
        .and_then(|s| s.get("Records"))
    else {
        panic!("POI records are missing");
    };
    assert_eq!(records.len(), 2);
    let tickets = |x| {
        let record = records
            .iter()
            .find(|record| record.get("pos") == Some(&Tag::IntArray(vec![x, 70, 2])))
            .unwrap();
        number(record.get("free_tickets"))
    };
    assert_eq!(tickets(1), Some(7));
    assert_eq!(tickets(2), Some(32));
    assert!(!root.path().join("region").exists());
    assert!(!root.path().join("entities").exists());
    assert!(!root.path().join("poi").exists());
}

#[test]
fn queued_backend_write_overrides_the_stored_chunk() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let save = SaveDir::with_backend(
        root.path().to_path_buf(),
        LevelShape::OVERWORLD,
        backend.clone(),
    );
    backend
        .chunks
        .lock()
        .unwrap()
        .insert((0, 0), Chunk::empty(0, 0));
    let (signal_started, started) = mpsc::channel();
    let (release, wait_for_release) = mpsc::channel();
    *backend.write_gate.lock().unwrap() = Some((signal_started, wait_for_release));
    let mut chunk = Chunk::empty(0, 0);
    chunk.set(1, 70, 2, 1);
    chunk.lit = true;
    save.save_later(&[Arc::new(chunk)], registry(), biomes())
        .unwrap();
    started
        .recv_timeout(horizon_std::time::Duration::from_secs(5))
        .unwrap();
    let queued = save.load(0, 0, registry(), biomes());
    let far = save.load_far(0, 0, registry(), biomes());
    release.send(()).unwrap();
    save.flush().unwrap();
    assert_eq!(queued.unwrap().get(1, 70, 2), 1);
    assert_eq!(far.unwrap().get(1, 70, 2), 1);
}

#[test]
fn backend_chunk_tags_decode_without_a_metadata_read() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let save = SaveDir::with_backend(
        root.path().to_path_buf(),
        LevelShape::OVERWORLD,
        backend.clone(),
    );
    let mut chunk = Chunk::empty(0, 0);
    chunk.set(1, 70, 2, 1);
    chunk.lit = true;
    backend
        .chunk_tags
        .lock()
        .unwrap()
        .insert((0, 0), encode(&chunk, registry(), biomes()));
    assert_eq!(
        save.load(0, 0, registry(), biomes()).unwrap().get(1, 70, 2),
        1
    );
    assert_eq!(
        save.load_far(0, 0, registry(), biomes())
            .unwrap()
            .get(1, 70, 2),
        1
    );
}

#[test]
fn native_backend_chunks_feed_far_loads_and_lod() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    let save = SaveDir::with_backend(
        root.path().to_path_buf(),
        LevelShape::OVERWORLD,
        backend.clone(),
    );
    let mut chunk = Chunk::empty(0, 0);
    chunk.set(1, 70, 2, 1);
    chunk.lit = true;
    chunk.light_loaded = true;
    backend.seed_tag(
        "region",
        (0, 0),
        encode_rest(&chunk, registry(), biomes(), None, save.shape),
    );
    backend.chunks.lock().unwrap().insert((0, 0), chunk);
    assert_eq!(
        save.load_far(0, 0, registry(), biomes())
            .unwrap()
            .get(1, 70, 2),
        1
    );
    let assets = crate::assets::Assets {
        registry: registry(),
        properties: tables().heightmaps,
        material_states: [BlockStateId(0); 6],
        blocks_fluid_flow: Default::default(),
    };
    let lod = save.lod_surfaces(0, 0, &assets, biomes()).unwrap();
    assert_eq!(lod[33].surface.unwrap().height, 71);
    assert_eq!(
        save.far_record(0, 0).unwrap().tag.get("sections"),
        Some(&Tag::List(Vec::new()))
    );
}

#[test]
fn encoded_rest_uses_calculated_blending_when_the_range_matches() {
    let mut chunk = Chunk::empty(0, 0);
    let mut extras = ChunkExtras::default();
    extras.root.insert(
        "blending_data".into(),
        Tag::compound([("min_section", Tag::Int(-4)), ("max_section", Tag::Int(20))]),
    );
    chunk.saved = Some(Arc::new(extras));
    let blending = BlendingData::unpack(-4, 20, Some(&[70.0; 16])).unwrap();
    let rest = encode_rest_saved(
        &chunk,
        registry(),
        biomes(),
        None,
        LevelShape::OVERWORLD,
        Some(&blending),
    );
    assert_eq!(
        rest.tag.get("blending_data").unwrap().get("heights"),
        Some(&Tag::List(vec![Tag::Float(70.0f32.to_bits()); 16]))
    );
}

#[test]
fn gateway_migration_leaves_backend_worlds_alone() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("horizon-end-gateways.json");
    fs::write(&legacy, b"not valid json").unwrap();
    let save = SaveDir::with_backend(
        root.path().to_path_buf(),
        LevelShape::OVERWORLD,
        Arc::new(MemoryBackend::default()),
    );
    assert_eq!(save.migrate_end_gateway_exits().unwrap(), 0);
    assert_eq!(fs::read(legacy).unwrap(), b"not valid json");
}

#[test]
fn old_chunk_bits_enumerates_backend_region_chunks() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(MemoryBackend::default());
    backend.seed_tag(
        "region",
        (33, 64),
        NamedTag {
            name: "".into(),
            tag: Tag::compound([
                ("DataVersion", Tag::Int(3465)),
                ("xPos", Tag::Int(33)),
                ("zPos", Tag::Int(64)),
            ]),
        },
    );
    let save = SaveDir::with_backend(
        root.path().to_path_buf(),
        LevelShape::OVERWORLD,
        backend.clone(),
    );
    let bits = save.old_chunk_bits(1, 2);
    assert_eq!(bits[0] & 2, 2);
    assert_eq!(backend.region_chunk_queries.load(Ordering::Relaxed), 1);
}

/// A folder whose store could not be opened fails its reads and writes instead of falling
/// back to region files, so nothing is written over the store.
#[test]
fn a_store_that_could_not_be_opened_fails_instead_of_falling_back() {
    let root = tempfile::tempdir().unwrap();
    let mut chunk = Chunk::empty(0, 0);
    chunk.set(1, 70, 2, 1);
    let anvil = SaveDir::new(root.path().to_path_buf(), LevelShape::OVERWORLD);
    let record = Record::Tag(Arc::new(encode(&chunk, registry(), biomes())));
    super::write_region_records(
        &anvil.root,
        &anvil.regions,
        "region",
        None,
        &[((0, 0), &record)],
    )
    .unwrap();
    let backend = chosen_backend(root.path(), Err("damaged index".into()));
    let save =
        SaveDir::with_optional_backend(root.path().to_path_buf(), LevelShape::OVERWORLD, backend);
    assert!(save.try_load(0, 0, registry(), biomes()).is_err());
    assert!(save.read_record_raw("region", 0, 0).is_none());
    assert!(
        super::write_region_records(
            &save.root,
            &save.regions,
            "region",
            save.backend.as_deref(),
            &[((0, 0), &Record::Delete)],
        )
        .is_err()
    );
    // The region file is still there and untouched.
    assert_eq!(
        anvil
            .load(0, 0, registry(), biomes())
            .unwrap()
            .get(1, 70, 2),
        1
    );
}

//! A soil's `canSustainPlant` answer against the plant's own rule, over an in-memory level. Needs
//! the prepared client resources (skipped when absent), as `tests` does.
use super::tests::{TestLevel, fixture};
use super::*;

#[test]
fn a_soils_answer_comes_before_the_plants_may_place_on() {
    use std::cell::RefCell;

    /// A level whose soil answers `canSustainPlant` for one block, and whose plant answers
    /// `mayPlaceOn`, recording what the soil was asked.
    struct Soil {
        inner: TestLevel,
        b: &'static Behaviours,
        /// A soil block and its answer.
        soil: Option<(BlockId, bool)>,
        /// A plant and the one block its own `mayPlaceOn` allows.
        plant: Option<(BlockId, BlockId)>,
        asked: RefCell<Vec<(BlockId, BlockId, BlockPos)>>,
    }
    impl BlockEntityNeighbors for Soil {}

    impl LevelAccess for Soil {
        fn block_state(&self, pos: BlockPos) -> BlockStateId {
            self.inner.block_state(pos)
        }
        fn set_raw(&mut self, pos: BlockPos, state: BlockStateId) -> Option<BlockStateId> {
            self.inner.set_raw(pos, state)
        }
        fn is_loaded(&self, pos: BlockPos) -> bool {
            self.inner.is_loaded(pos)
        }
        fn random(&mut self) -> &mut LegacyRandom {
            self.inner.random()
        }
        fn game_time(&self) -> i64 {
            self.inner.game_time()
        }
        fn emit(&mut self, event: LevelEvent) {
            self.inner.emit(event);
        }
        fn before_can_sustain_plant(
            &self,
            state: BlockStateId,
            ground: BlockStateId,
            ground_pos: BlockPos,
            sustains: &mut Option<bool>,
        ) {
            self.asked
                .borrow_mut()
                .push((self.b.block(state), self.b.block(ground), ground_pos));
            if let Some((soil, answer)) = self.soil
                && self.b.block(ground) == soil
            {
                *sustains = Some(answer);
            }
        }
        fn before_may_place_on(
            &self,
            state: BlockStateId,
            ground: BlockStateId,
            _ground_pos: BlockPos,
            may: &mut Option<bool>,
        ) {
            if let Some((plant, on)) = self.plant
                && self.b.block(state) == plant
            {
                *may = Some(self.b.block(ground) == on);
            }
        }
    }
    let Some(f) = fixture() else { return };
    let b = &f.behaviours;
    let state = |name: &str| f.registry.default_state(name).unwrap();
    let (dandelion, stone, grass, cobblestone) = (
        state("minecraft:dandelion"),
        state("minecraft:stone"),
        state("minecraft:grass_block"),
        state("minecraft:cobblestone"),
    );
    let mut level = Soil {
        inner: TestLevel::new(b),
        b,
        soil: None,
        plant: None,
        asked: RefCell::new(Vec::new()),
    };
    let mut ticks = Ticks::new(0);
    ticks.add_empty_chunk(ChunkPos::new(0, 0)).unwrap();
    let ground = BlockPos::new(1, 1, 1);
    let pos = BlockPos::new(1, 2, 1);
    let place = |level: &mut Soil, ticks: &mut Ticks| {
        let ctx = PlaceContext {
            clicked_pos: ground,
            face: Direction::Up,
            hit: horizon_core::geometry::Vec3::new(1.5, 2.0, 1.5),
            inside: false,
            yaw: 0.0,
            pitch: 0.0,
            sneaking: false,
            item: "minecraft:dandelion".into(),
            can_use_game_master_blocks: false,
        };
        b.place_block(level, ticks, &ctx)
    };

    // Default: no soil answers, so vanilla's rule runs. A dandelion stands on grass and not on stone.
    level.set_raw(ground, grass);
    assert!(b.can_survive(&mut level, &mut ticks, pos, dandelion));
    level.set_raw(ground, stone);
    assert!(!b.can_survive(&mut level, &mut ticks, pos, dandelion));
    // The soil is asked for the plant above it, at its own position.
    assert_eq!(
        level.asked.borrow().last().copied(),
        Some((b.block(dandelion), b.block(stone), ground))
    );
    assert_eq!(
        place(&mut level, &mut ticks),
        PlaceOutcome::Failed,
        "no answer from the soil refuses the dandelion on stone"
    );

    // Allow: the soil lets the dandelion stand on stone, so it is placed there.
    level.soil = Some((b.block(stone), true));
    assert!(b.can_survive(&mut level, &mut ticks, pos, dandelion));
    assert!(matches!(
        place(&mut level, &mut ticks),
        PlaceOutcome::Placed { pos: p, .. } if p == pos
    ));
    assert_eq!(level.block_state(pos), dandelion);
    // A soil that answers for stone leaves other soil to vanilla.
    level.set_raw(pos, b.air());
    level.set_raw(ground, grass);
    assert!(b.can_survive(&mut level, &mut ticks, pos, dandelion));
    level.set_raw(ground, cobblestone);
    assert!(!b.can_survive(&mut level, &mut ticks, pos, dandelion));

    // Deny: the soil refuses a dandelion on grass, where vanilla allows it.
    level.soil = Some((b.block(grass), false));
    level.set_raw(ground, grass);
    assert!(!b.can_survive(&mut level, &mut ticks, pos, dandelion));
    assert_eq!(place(&mut level, &mut ticks), PlaceOutcome::Failed);
    // A planted dandelion pops off when its ground turns to a soil that refuses it.
    level.soil = None;
    level.set_raw(pos, dandelion);
    level.soil = Some((b.block(grass), false));
    level.set_raw(ground, stone);
    b.set_block(&mut level, &mut ticks, ground, grass, flags::UPDATE_ALL);
    assert_eq!(level.block_state(pos), b.air());

    // The soil's answer comes before the plant's own: the plant allows only stone, the soil
    // refuses stone and allows grass, and the soil's answer wins both ways.
    level.set_raw(pos, b.air());
    level.plant = Some((b.block(dandelion), b.block(stone)));
    level.soil = Some((b.block(stone), false));
    level.set_raw(ground, stone);
    assert!(!b.can_survive(&mut level, &mut ticks, pos, dandelion));
    level.soil = Some((b.block(grass), true));
    level.set_raw(ground, grass);
    assert!(b.can_survive(&mut level, &mut ticks, pos, dandelion));
    // With no answer from that soil the plant's `mayPlaceOn` decides.
    level.soil = Some((b.block(cobblestone), true));
    level.set_raw(ground, stone);
    assert!(b.can_survive(&mut level, &mut ticks, pos, dandelion));
    level.set_raw(ground, grass);
    assert!(!b.can_survive(&mut level, &mut ticks, pos, dandelion));
}

#[test]
fn declared_growing_plant_pairs_survive_and_pop_in_both_directions() {
    let Some(f) = fixture() else { return };
    let root = horizon_std::env::var_os("HORIZON_RESOURCES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.local/horizon-client")
                .join(horizon_core::VANILLA_VERSION)
        });
    let mut b = Behaviours::load(&f.registry, &root).expect("behaviours");
    let id = |name: &str| b.block(f.registry.default_state(name).expect(name));
    let stone = f.registry.default_state("minecraft:stone").unwrap();
    let air = b.air();
    let down_head = id("minecraft:weeping_vines");
    let down_body = id("minecraft:twisting_vines_plant");
    let up_head = id("minecraft:twisting_vines");
    let up_body = id("minecraft:weeping_vines_plant");
    let kelp_head = id("minecraft:kelp");
    let kelp_body = id("minecraft:kelp_plant");

    let set_family =
        |b: &mut Behaviours, head: BlockId, body: BlockId, direction: Direction, kelp: bool| {
            for block in [head, body] {
                let data = &mut b.blocks[block.0 as usize];
                data.growth_direction = direction;
                data.growing_plant_head = Some(head);
                data.growing_plant_body = Some(body);
                data.growing_plant_kelp = kelp;
            }
        };
    set_family(&mut b, down_head, down_body, Direction::Down, false);
    set_family(&mut b, up_head, up_body, Direction::Up, false);

    let mut level = TestLevel::new(&b);
    let mut ticks = Ticks::new(0);
    ticks.add_empty_chunk(ChunkPos::new(0, 0)).unwrap();

    for (head, body, direction, x) in [
        (up_head, up_body, Direction::Up, 2),
        (down_head, down_body, Direction::Down, 6),
    ] {
        let (support_pos, head_pos, body_pos) = if direction == Direction::Up {
            (
                BlockPos::new(x, 1, 2),
                BlockPos::new(x, 2, 2),
                BlockPos::new(x, 3, 2),
            )
        } else {
            (
                BlockPos::new(x, 5, 2),
                BlockPos::new(x, 4, 2),
                BlockPos::new(x, 3, 2),
            )
        };
        level.set_raw(support_pos, stone);
        level.set_raw(head_pos, b.table.default_state(head));
        level.set_raw(body_pos, b.table.default_state(body));
        let head_state = level.block_state(head_pos);
        let body_state = level.block_state(body_pos);
        assert!(b.can_survive(&mut level, &mut ticks, head_pos, head_state));
        assert!(b.can_survive(&mut level, &mut ticks, body_pos, body_state));

        b.set_block(&mut level, &mut ticks, support_pos, air, flags::UPDATE_ALL);
        assert_eq!(
            level.block_state(head_pos),
            air,
            "head should pop: {direction:?}"
        );
        assert_eq!(
            level.block_state(body_pos),
            air,
            "body should pop: {direction:?}"
        );
    }

    let forbidden_support = (0..b.table.block_count())
        .map(|n| BlockId(n as u16))
        .find(|block| {
            b.tags.cannot_support_kelp.contains(*block)
                && b.is_face_sturdy(b.table.default_state(*block), Direction::Up)
        })
        .expect("the vanilla cannot_support_kelp tag has a sturdy support block");
    let forbidden_state = b.table.default_state(forbidden_support);
    let support_pos = BlockPos::new(10, 1, 2);
    let head_pos = BlockPos::new(10, 2, 2);
    level.set_raw(support_pos, forbidden_state);
    level.set_raw(head_pos, b.table.default_state(kelp_head));
    let head_state = level.block_state(head_pos);

    set_family(&mut b, kelp_head, kelp_body, Direction::Up, false);
    assert!(
        b.can_survive(&mut level, &mut ticks, head_pos, head_state),
        "a declared family with kelp=false must allow tagged supports"
    );

    set_family(&mut b, kelp_head, kelp_body, Direction::Up, true);
    assert!(
        !b.can_survive(&mut level, &mut ticks, head_pos, head_state),
        "kelp-like custom families must reject tagged supports"
    );

    for block in [kelp_head, kelp_body] {
        let data = &mut b.blocks[block.0 as usize];
        data.growing_plant_head = None;
        data.growing_plant_body = None;
        data.growing_plant_kelp = false;
    }
    assert!(
        !b.can_survive(&mut level, &mut ticks, head_pos, head_state),
        "vanilla kelp must keep its class-based support restriction"
    );
}

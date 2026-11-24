//! Every path that can change a stored block entity gives its position a new
//! revision; reads keep it.
use super::*;

fn chest(n: usize) -> BlockEntity {
    BlockEntity::Chest(vec![None; n])
}

fn store() -> BlockEntities {
    let mut b = BlockEntities::default();
    b.load_chunk(vec![
        ((0, 64, 0), chest(27)),
        ((1, 64, 0), chest(27)),
        ((17, 64, 0), chest(5)),
    ]);
    b
}

/// Runs `change` and reports whether the revision at `pos` moved.
fn moves(b: &mut BlockEntities, pos: Pos, change: impl FnOnce(&mut BlockEntities)) -> bool {
    let before = b.revision(&pos);
    change(b);
    let after = b.revision(&pos);
    assert!(after.is_some(), "{pos:?} keeps a revision while stored");
    before != after
}

#[test]
fn loaded_entities_have_distinct_revisions() {
    let b = store();
    let a = b.revision(&(0, 64, 0)).unwrap();
    let c = b.revision(&(1, 64, 0)).unwrap();
    assert_ne!(a, c);
    assert_eq!(b.revision(&(5, 5, 5)), None);
    // Revisions come from one process-wide counter: another store never repeats them.
    let other = store();
    assert_ne!(other.revision(&(0, 64, 0)), Some(a));
}

#[test]
fn reads_keep_the_revision() {
    let mut b = store();
    let pos = (0, 64, 0);
    let before = b.revision(&pos);
    assert!(b.get(&pos).is_some());
    assert_eq!(b.iter().count(), 3);
    assert_eq!((&b).into_iter().count(), 3);
    assert_eq!(b.sign_editor(&pos), None);
    b.sleep(pos);
    assert!(b.is_asleep(&pos));
    assert!(b.generation(&pos).is_some());
    let _ = b.listed(Listed::Beacon).count();
    let _ = b.in_chunk((0, 0)).count();
    assert_eq!(b.revision(&pos), before);
}

#[test]
fn every_mutable_path_moves_the_revision() {
    let mut b = store();
    let pos = (0, 64, 0);
    assert!(moves(&mut b, pos, |b| {
        b.get_mut(&pos);
    }));
    assert!(moves(&mut b, pos, |b| {
        b.iter_mut().for_each(drop);
    }));
    assert!(moves(&mut b, pos, |b| {
        b.values_mut().for_each(drop);
    }));
    // A payload rewrite of the same type keeps the identity but not the revision.
    assert!(moves(&mut b, pos, |b| {
        let generation = b.generation(&pos);
        b.insert(pos, chest(27));
        assert_eq!(b.generation(&pos), generation);
    }));
    assert!(moves(&mut b, pos, |b| {
        b.replace(pos, chest(27));
    }));
    assert!(moves(&mut b, pos, |b| {
        b.get_or_insert_with(pos, || unreachable!("present"));
    }));
    assert!(moves(&mut b, pos, |b| {
        let (lease, entity) = b.take(&pos).unwrap();
        assert!(b.put(lease, entity));
    }));
    assert!(moves(&mut b, pos, |b| b.retain(|_, _| true)));
    assert!(moves(&mut b, pos, |b| b.load_chunk(vec![(pos, chest(27))])));
    let sign = (2, 64, 0);
    b.insert(sign, BlockEntity::for_block("minecraft:oak_sign").unwrap());
    assert!(moves(&mut b, sign, |b| b.set_sign_editor(sign, Some(7))));
    // A carried entity lands as a new write.
    assert!(b.carry(pos, (0, 65, 0)));
    assert_eq!(b.revision(&pos), None);
    assert_eq!(b.revision(&(0, 65, 0)), None);
    assert!(b.land((0, 65, 0)));
    assert!(b.revision(&(0, 65, 0)).is_some());
}

#[test]
fn removal_forgets_the_revision() {
    let mut b = store();
    let a = (0, 64, 0);
    let rev = b.revision(&a);
    b.remove(&a);
    assert_eq!(b.revision(&a), None);
    b.insert(a, chest(27));
    assert_ne!(b.revision(&a), rev, "a new entity at the same place is new");
    let (_lease, _entity) = b.take(&a).unwrap();
    assert_eq!(b.revision(&a), None, "out of the map for its tick");
    b.retain(|p, _| *p != (1, 64, 0));
    assert_eq!(b.revision(&(1, 64, 0)), None);
    b.clear();
    assert_eq!(b.revision(&(17, 64, 0)), None);
}

#[test]
fn unloading_others_keeps_the_revision() {
    let mut b = store();
    let kept = (0, 64, 0);
    let rev = b.revision(&kept);
    b.unload(|p| *p == (1, 64, 0));
    assert_eq!(b.revision(&(1, 64, 0)), None);
    assert_eq!(b.revision(&kept), rev);
    b.unload_chunks(&[(1, 0)]);
    assert_eq!(b.revision(&(17, 64, 0)), None);
    assert_eq!(b.revision(&kept), rev);
}

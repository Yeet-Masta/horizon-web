//! The block entity map with a revision per position. Every path to a
//! `&mut BlockEntity` goes through this type and gives that position a new
//! revision, so a save can reuse what it wrote for an unchanged block entity.
//! The map is only readable from outside (`Deref`): there is no `DerefMut`.
use super::{BlockEntity, Pos, next_token};
use crate::fast_hash::FxHashMap;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Revised {
    map: HashMap<Pos, BlockEntity>,
    /// A token from the process-wide counter, new for each write: a position
    /// and revision pair is never reused, by this store or any other.
    revisions: FxHashMap<Pos, u64>,
}

impl std::ops::Deref for Revised {
    type Target = HashMap<Pos, BlockEntity>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl Revised {
    fn touch(&mut self, pos: Pos) {
        self.revisions.insert(pos, next_token());
    }
    pub(super) fn revision(&self, pos: &Pos) -> Option<u64> {
        self.revisions.get(pos).copied()
    }
    pub(super) fn get_mut(&mut self, pos: &Pos) -> Option<&mut BlockEntity> {
        let entity = self.map.get_mut(pos)?;
        self.revisions.insert(*pos, next_token());
        Some(entity)
    }
    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (&Pos, &mut BlockEntity)> {
        self.touch_all();
        self.map.iter_mut()
    }
    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut BlockEntity> {
        self.touch_all();
        self.map.values_mut()
    }
    fn touch_all(&mut self) {
        for pos in self.map.keys() {
            self.revisions.insert(*pos, next_token());
        }
    }
    pub(super) fn insert(&mut self, pos: Pos, entity: BlockEntity) -> Option<BlockEntity> {
        self.touch(pos);
        self.map.insert(pos, entity)
    }
    pub(super) fn get_or_insert_with(
        &mut self,
        pos: Pos,
        make: impl FnOnce() -> BlockEntity,
    ) -> &mut BlockEntity {
        self.touch(pos);
        self.map.entry(pos).or_insert_with(make)
    }
    pub(super) fn remove(&mut self, pos: &Pos) -> Option<BlockEntity> {
        self.revisions.remove(pos);
        self.map.remove(pos)
    }
    /// Keeps the entries `keep` accepts; each kept one is new, as `keep` could
    /// change it. Returns the positions it dropped.
    pub(super) fn retain(
        &mut self,
        mut keep: impl FnMut(&Pos, &mut BlockEntity) -> bool,
    ) -> Vec<Pos> {
        let mut dropped = Vec::new();
        let revisions = &mut self.revisions;
        self.map.retain(|pos, entity| {
            let kept = keep(pos, entity);
            if kept {
                revisions.insert(*pos, next_token());
            } else {
                revisions.remove(pos);
                dropped.push(*pos);
            }
            kept
        });
        dropped
    }
    /// [`Self::retain`] by position alone: the kept entries keep their revision.
    pub(super) fn retain_keys(&mut self, mut keep: impl FnMut(&Pos) -> bool) -> Vec<Pos> {
        let mut dropped = Vec::new();
        let revisions = &mut self.revisions;
        self.map.retain(|pos, _| {
            let kept = keep(pos);
            if !kept {
                revisions.remove(pos);
                dropped.push(*pos);
            }
            kept
        });
        dropped
    }
    pub(super) fn clear(&mut self) {
        self.revisions.clear();
        self.map.clear();
    }
}

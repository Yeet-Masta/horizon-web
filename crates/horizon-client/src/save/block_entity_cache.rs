//! Lazy cache of the block entity tags a `SaveDir` wrote, for repeated saves.
//!
//! A saved block entity is a function of the live block entity, its block's
//! name, its `CustomName`, and the source NBT it was loaded with. The live
//! entity is matched by its store revision (`BlockEntities::revision`), which
//! changes whenever the store lends it mutably; the other three are compared
//! exactly. A hit returns the tag the encoder made for the same inputs before
//! the per-save overlays (stack limit, attachments, pending loot), which the
//! caller still applies. The cache fills on saves, never on load.
//!
//! Entries hold serialized NBT, a tenth of the memory of the tag tree, and a
//! hit reads it back: still several times cheaper than encoding and merging.
//! A source equal to what was saved, the usual case, is not stored twice.
//!
//! A save that finds no tag notes the entity's revision, and serializes the tag
//! only when the revision is the one the last save noted: the entity stayed the
//! same for a whole save interval. So a first save costs next to nothing, the
//! second keeps the tags, the third reuses them, and an entity that changes every
//! save is never serialized. A full shard evicts only tags no save has used for
//! `PROTECT` (longer than an autosave interval), the oldest it finds in a few
//! steps of a ring; finding none, it keeps what it has and skips the new tag
//! before serializing it. So a working set larger than the cap does not churn.
use crate::containers::BlockEntity;
use horizon_core::nbt::{self, Limits, NamedTag, Tag};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Pos = (i32, i32, i32);

/// Default per-`SaveDir` byte cap for retained block entity tags.
pub(super) const DEFAULT_BYTES: usize = 64 * 1024 * 1024;
/// Entries larger than this (with their source) are not kept.
const ENTRY_BYTES: usize = 64 * 1024;
const SHARDS: usize = 16;
/// Ring steps one insert may take looking for an entry to evict.
const SWEEP: usize = 8;
/// How long a used tag is kept from eviction: two vanilla autosave intervals.
const PROTECT: Duration = Duration::from_secs(600);
/// Noted revisions per shard (about 24 bytes each), outside the byte cap.
const NOTES: usize = 8 * 1024;

/// The source NBT an entry was made with.
enum Source {
    None,
    /// Equal to the saved tag.
    Saved,
    Other(Arc<[u8]>),
}

struct Entry {
    revision: u64,
    block: Box<str>,
    name: Option<Tag>,
    source: Source,
    saved: Arc<[u8]>,
    /// When a save last kept or reused it.
    used: Instant,
    bytes: usize,
}

/// A revision a save saw without keeping a tag.
#[derive(Clone, Copy)]
struct Note {
    revision: u64,
    /// Its tag was too large to keep: not serialized again at this revision.
    oversize: bool,
}

#[derive(Default)]
struct Shard {
    entries: crate::fast_hash::FxHashMap<Pos, Entry>,
    /// Entry positions in the order eviction looks at them; it can hold
    /// positions whose entry is gone or was put back, which it skips.
    ring: std::collections::VecDeque<Pos>,
    bytes: usize,
    notes: crate::fast_hash::FxHashMap<Pos, Note>,
}

impl Shard {
    fn remove(&mut self, pos: &Pos) -> Option<Entry> {
        let entry = self.entries.remove(pos)?;
        self.bytes -= entry.bytes;
        Some(entry)
    }
    /// Evicts one entry unused for `protect`, looking at most `SWEEP` places
    /// round the ring; false when it found none.
    fn evict_one(&mut self, protect: Duration) -> bool {
        for _ in 0..SWEEP.min(self.ring.len()) {
            let Some(pos) = self.ring.pop_front() else {
                return false;
            };
            match self.entries.get(&pos) {
                None => {}
                Some(entry) if entry.used.elapsed() < protect => self.ring.push_back(pos),
                Some(_) => {
                    self.remove(&pos);
                    return true;
                }
            }
        }
        false
    }
    fn note(&mut self, pos: Pos, note: Note) {
        if self.notes.len() >= NOTES
            && !self.notes.contains_key(&pos)
            && let Some(other) = self.notes.keys().next().copied()
        {
            self.notes.remove(&other);
        }
        self.notes.insert(pos, note);
    }
}

pub(super) struct BlockEntityCache {
    shards: [Mutex<Shard>; SHARDS],
    shard_bytes: usize,
    protect: Duration,
    #[cfg(test)]
    hits: std::sync::atomic::AtomicUsize,
}

/// The inputs a saved block entity is made from, besides the entity itself.
pub(super) struct Inputs<'a> {
    pub block: &'a str,
    pub name: Option<&'a Tag>,
    pub source: Option<&'a Tag>,
}

/// The kinds whose saves the cache tests check byte for byte against an
/// uncached encode (containers, furnaces, signs, banners, skulls). Others,
/// mods' included, take the normal path until a test covers them.
pub(super) fn eligible(entity: &BlockEntity) -> bool {
    matches!(
        entity,
        BlockEntity::Chest(_)
            | BlockEntity::Furnace(_)
            | BlockEntity::Sign(_)
            | BlockEntity::Banner(_)
            | BlockEntity::Skull(_)
    )
}

fn write(tag: Tag) -> (Option<Arc<[u8]>>, Tag) {
    let root = NamedTag {
        name: Default::default(),
        tag,
    };
    let bytes = nbt::write_named(&root, Limits::saved()).ok();
    (bytes.map(Arc::from), root.tag)
}

fn has_negative_zero(tag: &Tag) -> bool {
    let mut stack = vec![tag];
    while let Some(tag) = stack.pop() {
        match tag {
            Tag::Float(bits) if *bits == (-0.0f32).to_bits() => return true,
            Tag::Double(bits) if *bits == (-0.0f64).to_bits() => return true,
            Tag::List(items) => stack.extend(items.iter()),
            Tag::Compound(map) => stack.extend(map.values()),
            _ => {}
        }
    }
    false
}

fn tag_bytes(tag: &Tag) -> usize {
    let mut total = std::mem::size_of::<Tag>();
    let mut stack = vec![tag];
    while let Some(tag) = stack.pop() {
        match tag {
            Tag::ByteArray(v) => total += v.len(),
            Tag::String(s) => total += s.0.len() * std::mem::size_of::<u16>(),
            Tag::IntArray(v) => total += v.len() * std::mem::size_of::<i32>(),
            Tag::LongArray(v) => total += v.len() * std::mem::size_of::<i64>(),
            Tag::List(items) => {
                total += items.len() * std::mem::size_of::<Tag>();
                stack.extend(items.iter());
            }
            Tag::Compound(map) => {
                for (k, v) in map {
                    total += std::mem::size_of::<(horizon_core::nbt::JavaString, Tag)>()
                        + k.0.len() * std::mem::size_of::<u16>();
                    stack.push(v);
                }
            }
            _ => {}
        }
    }
    total
}

fn read(bytes: &[u8]) -> Option<Tag> {
    nbt::read_named(bytes, Limits::saved())
        .ok()
        .map(|(root, _)| root.tag)
}

impl BlockEntityCache {
    pub(super) fn new(bytes: usize) -> Self {
        Self::protecting(bytes, PROTECT)
    }
    pub(super) fn protecting(bytes: usize, protect: Duration) -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::default()),
            shard_bytes: bytes / SHARDS,
            protect,
            #[cfg(test)]
            hits: Default::default(),
        }
    }
    fn shard(&self, pos: Pos) -> std::sync::MutexGuard<'_, Shard> {
        let hash = (pos.0 as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add((pos.1 as u32).wrapping_mul(0x85EB_CA6B))
            .wrapping_add((pos.2 as u32).wrapping_mul(0xC2B2_AE35));
        self.shards[(hash >> 28) as usize % SHARDS]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    /// The tag saved for `pos` at `revision` from the same inputs, if kept.
    pub(super) fn get(&self, pos: Pos, revision: u64, inputs: &Inputs<'_>) -> Option<Tag> {
        let mut shard = self.shard(pos);
        let entry = shard.entries.get_mut(&pos)?;
        if entry.revision != revision
            || *entry.block != *inputs.block
            || entry.name.as_ref() != inputs.name
        {
            return None;
        }
        entry.used = Instant::now();
        let saved = Arc::clone(&entry.saved);
        let source = match &entry.source {
            Source::None => None,
            Source::Saved => Some(None),
            Source::Other(bytes) => Some(Some(Arc::clone(bytes))),
        };
        drop(shard);
        let saved = read(&saved)?;
        let same = match (source, inputs.source) {
            (None, None) => true,
            (Some(None), Some(live)) => *live == saved,
            (Some(Some(bytes)), Some(live)) => read(&bytes).is_some_and(|kept| kept == *live),
            _ => false,
        };
        #[cfg(test)]
        self.hits
            .fetch_add(usize::from(same), std::sync::atomic::Ordering::Relaxed);
        same.then_some(saved)
    }
    /// After a miss: note `revision` for `pos`, or keep `saved`, made at it
    /// from `inputs`, when the last save noted the same. Gives `saved` back.
    pub(super) fn put(&self, pos: Pos, revision: u64, inputs: &Inputs<'_>, saved: Tag) -> Tag {
        let note = |oversize| Note { revision, oversize };
        {
            let mut shard = self.shard(pos);
            let kept_at = shard.remove(&pos).map(|old| old.revision);
            let noted = shard.notes.remove(&pos);
            let oversize = noted.is_some_and(|n| n.revision == revision && n.oversize);
            let stable = !oversize
                && (kept_at == Some(revision) || noted.is_some_and(|n| n.revision == revision));
            if !stable || !(shard.bytes < self.shard_bytes || shard.evict_one(self.protect)) {
                shard.note(pos, note(oversize));
                return saved;
            }
        }
        if has_negative_zero(&saved) || inputs.source.is_some_and(has_negative_zero) {
            return saved;
        }
        let source = match inputs.source {
            None => Some(Source::None),
            Some(source) if *source == saved => Some(Source::Saved),
            Some(source) => write(source.clone()).0.map(Source::Other),
        };
        let (bytes, saved) = write(saved);
        let (Some(source), Some(bytes)) = (source, bytes) else {
            return saved;
        };
        let size = std::mem::size_of::<Entry>()
            + inputs.block.len()
            + bytes.len()
            + match &source {
                Source::Other(source) => source.len(),
                _ => 0,
            }
            + inputs.name.map_or(0, tag_bytes);
        let mut shard = self.shard(pos);
        shard.remove(&pos);
        let oversize = size > ENTRY_BYTES;
        let mut room = !oversize;
        while room && shard.bytes + size > self.shard_bytes {
            room = shard.evict_one(self.protect);
        }
        if !room {
            shard.note(pos, note(oversize));
            return saved;
        }
        shard.bytes += size;
        // A replaced entry leaves its place in the ring: rebuild it before
        // stale places outnumber the entries.
        if shard.ring.len() > 2 * shard.entries.len() + SWEEP {
            shard.ring = shard.entries.keys().copied().collect();
        }
        shard.ring.push_back(pos);
        let entry = Entry {
            revision,
            block: inputs.block.into(),
            name: inputs.name.cloned(),
            source,
            saved: bytes,
            used: Instant::now(),
            bytes: size,
        };
        shard.entries.insert(pos, entry);
        saved
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap().entries.len())
            .sum()
    }
    #[cfg(test)]
    pub(super) fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().bytes).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_zero_is_not_admitted_to_the_cache() {
        let cache = BlockEntityCache::new(DEFAULT_BYTES);
        let pos = (1, 64, 2);
        let inputs = Inputs {
            block: "minecraft:furnace",
            name: None,
            source: None,
        };
        let saved = Tag::compound([("speed", Tag::Float((-0.0f32).to_bits()))]);

        let first = cache.put(pos, 7, &inputs, saved.clone());
        let second = cache.put(pos, 7, &inputs, saved.clone());
        assert_eq!(first, saved);
        assert_eq!(second, saved);
        assert_eq!(cache.len(), 0);
        assert!(cache.get(pos, 7, &inputs).is_none());
    }

    #[test]
    fn oversize_custom_name_is_charged_and_rejected() {
        let cache = BlockEntityCache::new(DEFAULT_BYTES);
        let pos = (3, 64, 4);
        let long_name = Tag::String("a".repeat(40_000).as_str().into());
        let inputs = Inputs {
            block: "minecraft:chest",
            name: Some(&long_name),
            source: None,
        };
        let saved = Tag::compound([("id", Tag::String("minecraft:chest".into()))]);

        cache.put(pos, 1, &inputs, saved.clone());
        cache.put(pos, 1, &inputs, saved);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
    }
}

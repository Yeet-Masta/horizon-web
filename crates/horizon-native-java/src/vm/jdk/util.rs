//! `java.util`: the collections with Java's own algorithms (so iteration
//! order is HotSpot's: HashMap's hash spreading, table growth and
//! treeify-or-resize, PriorityQueue's heap layout), iterators, Arrays,
//! Collections, Objects, Optional, Random, comparators and the functional
//! interfaces, StringJoiner, regex and the atomics.
use super::{A, FINAL, INTERFACE, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, state_mut, this, void, with_state};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Array, Data, Exc, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

const NIL: u32 = u32::MAX;

// ------------------------------------------------------------------ JList

#[derive(Debug, Clone, Default)]
pub struct JList {
    pub items: Vec<Value>,
    pub mod_count: u32,
    /// Refuses changes: `List.of`, `Collections.unmodifiableList`...
    pub immutable: bool,
    /// Refuses size changes but allows `set` (`Arrays.asList`).
    pub fixed: bool,
    /// The array an `Arrays.asList` list writes through to.
    pub backing: ObjId,
}

// ------------------------------------------------------------------ JMap

#[derive(Debug, Clone)]
pub struct MapEntry {
    pub hash: i32,
    pub key: Value,
    pub value: Value,
    before: u32,
    after: u32,
}

/// `java.util.HashMap`, entry for entry: the same table sizes, the same
/// bins, the same order within a bin, so iteration matches HotSpot's.
#[derive(Debug, Clone)]
pub struct JMap {
    table: Vec<Vec<u32>>,
    entries: Vec<Option<MapEntry>>,
    free: Vec<u32>,
    pub size: usize,
    threshold: usize,
    load_factor: f32,
    /// LinkedHashMap: iteration in insertion (or access) order.
    pub linked: bool,
    pub access_order: bool,
    head: u32,
    tail: u32,
    pub mod_count: u32,
    /// IdentityHashMap: keys compare by reference, hashed by identity.
    pub identity: bool,
    pub immutable: bool,
    /// A set backed by this map (HashSet...): values are a marker.
    pub set: bool,
    /// A LinkedHashMap's `removeEldestEntry` is overridden: ask after a put.
    pub check_eldest: bool,
}

fn table_size_for(cap: usize) -> usize {
    if cap <= 1 { 1 } else { cap.next_power_of_two().min(1 << 30) }
}

impl JMap {
    pub fn new() -> JMap {
        JMap::with_capacity(None, 0.75)
    }

    pub fn with_capacity(initial: Option<usize>, load_factor: f32) -> JMap {
        JMap {
            table: Vec::new(),
            entries: Vec::new(),
            free: Vec::new(),
            size: 0,
            threshold: initial.map_or(0, table_size_for),
            load_factor,
            linked: false,
            access_order: false,
            head: NIL,
            tail: NIL,
            mod_count: 0,
            identity: false,
            immutable: false,
            set: false,
            check_eldest: false,
        }
    }

    /// `HashMap.hash`: the key's hash with its high bits spread down.
    pub fn spread(h: i32) -> i32 {
        h ^ ((h as u32) >> 16) as i32
    }

    fn resize(&mut self) {
        let old_cap = self.table.len();
        let (new_cap, new_thr) = if old_cap > 0 {
            if old_cap >= 1 << 30 {
                self.threshold = i32::MAX as usize;
                return;
            }
            (old_cap * 2, if self.threshold > 0 && old_cap >= 16 { self.threshold * 2 } else { ((old_cap * 2) as f32 * self.load_factor) as usize })
        } else if self.threshold > 0 {
            let cap = self.threshold;
            (cap, (cap as f32 * self.load_factor) as usize)
        } else {
            (16, 12)
        };
        let new_thr = if old_cap > 0 && !(self.threshold > 0 && old_cap >= 16) { (new_cap as f32 * self.load_factor) as usize } else { new_thr };
        self.threshold = new_thr;
        let old = std::mem::take(&mut self.table);
        self.table = vec![Vec::new(); new_cap];
        // Each bin splits into a low and a high bin, keeping its order.
        for bin in old {
            for e in bin {
                let h = self.entries[e as usize].as_ref().unwrap().hash;
                self.table[(h as u32 as usize) & (new_cap - 1)].push(e);
            }
        }
    }

    pub fn values(&self) -> impl Iterator<Item = (Value, Value)> + '_ {
        self.entries.iter().flatten().map(|e| (e.key, e.value))
    }

    /// The entry slots in iteration order.
    pub fn order(&self) -> Vec<u32> {
        if self.linked {
            let mut out = Vec::with_capacity(self.size);
            let mut at = self.head;
            while at != NIL {
                out.push(at);
                at = self.entries[at as usize].as_ref().unwrap().after;
            }
            out
        } else {
            self.table.iter().flatten().copied().collect()
        }
    }

    pub fn entry(&self, slot: u32) -> Option<&MapEntry> {
        self.entries.get(slot as usize).and_then(Option::as_ref)
    }

    fn link_last(&mut self, slot: u32) {
        let tail = self.tail;
        if let Some(e) = self.entries[slot as usize].as_mut() {
            e.before = tail;
            e.after = NIL;
        }
        if tail == NIL {
            self.head = slot;
        } else if let Some(t) = self.entries[tail as usize].as_mut() {
            t.after = slot;
        }
        self.tail = slot;
    }

    fn unlink(&mut self, slot: u32) {
        let (before, after) = {
            let e = self.entries[slot as usize].as_ref().unwrap();
            (e.before, e.after)
        };
        if before == NIL {
            self.head = after;
        } else if let Some(b) = self.entries[before as usize].as_mut() {
            b.after = after;
        }
        if after == NIL {
            self.tail = before;
        } else if let Some(a) = self.entries[after as usize].as_mut() {
            a.before = before;
        }
    }

    /// LinkedHashMap with access order: an accessed entry moves to the end.
    pub fn touch(&mut self, slot: u32) {
        if self.linked && self.access_order && self.tail != slot {
            self.unlink(slot);
            self.link_last(slot);
            self.mod_count += 1;
        }
    }

    /// The bin and position of a key, comparing with `eq`.
    pub fn find(&self, hash: i32, mut eq: impl FnMut(Value) -> Res<bool>) -> Res<Option<u32>> {
        if self.table.is_empty() {
            return Ok(None);
        }
        let bin = (hash as u32 as usize) & (self.table.len() - 1);
        for &slot in &self.table[bin] {
            let e = self.entries[slot as usize].as_ref().unwrap();
            if e.hash == hash && eq(e.key)? {
                return Ok(Some(slot));
            }
        }
        Ok(None)
    }

    /// Adds a new mapping (the key is known absent), growing as HashMap does.
    pub fn insert(&mut self, hash: i32, key: Value, value: Value) -> u32 {
        if self.table.is_empty() {
            self.resize();
        }
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.entries.push(None);
                self.entries.len() as u32 - 1
            }
        };
        self.entries[slot as usize] = Some(MapEntry {
            hash,
            key,
            value,
            before: NIL,
            after: NIL,
        });
        let bin = (hash as u32 as usize) & (self.table.len() - 1);
        self.table[bin].push(slot);
        if self.linked {
            self.link_last(slot);
        }
        // A bin of eight: HashMap treeifies it, or grows a small table instead.
        if self.table[bin].len() > 8 && self.table.len() < 64 {
            self.resize();
        }
        self.mod_count += 1;
        self.size += 1;
        if self.size > self.threshold {
            self.resize();
        }
        slot
    }

    /// Adds a new mapping at the head of its bin, as `HashMap.compute`,
    /// `computeIfAbsent` and `merge` do (growing first when over the threshold).
    pub fn insert_front(&mut self, hash: i32, key: Value, value: Value) -> u32 {
        if self.table.is_empty() || self.size > self.threshold {
            self.resize();
        }
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.entries.push(None);
                self.entries.len() as u32 - 1
            }
        };
        self.entries[slot as usize] = Some(MapEntry {
            hash,
            key,
            value,
            before: NIL,
            after: NIL,
        });
        let bin = (hash as u32 as usize) & (self.table.len() - 1);
        self.table[bin].insert(0, slot);
        if self.linked {
            self.link_last(slot);
        }
        if self.table[bin].len() >= 8 && self.table.len() < 64 {
            self.resize();
        }
        self.mod_count += 1;
        self.size += 1;
        slot
    }

    pub fn set_value(&mut self, slot: u32, value: Value) -> Value {
        let e = self.entries[slot as usize].as_mut().unwrap();
        std::mem::replace(&mut e.value, value)
    }

    pub fn remove_slot(&mut self, slot: u32) -> Option<MapEntry> {
        let e = self.entries[slot as usize].take()?;
        let bin = (e.hash as u32 as usize) & (self.table.len() - 1);
        self.table[bin].retain(|&s| s != slot);
        if self.linked {
            // Unlinking needs the entry back for a moment.
            self.entries[slot as usize] = Some(e.clone());
            self.unlink(slot);
            self.entries[slot as usize] = None;
        }
        self.free.push(slot);
        self.size -= 1;
        self.mod_count += 1;
        Some(e)
    }

    pub fn clear(&mut self) {
        if self.size > 0 {
            self.mod_count += 1;
        }
        for bin in &mut self.table {
            bin.clear();
        }
        self.entries.clear();
        self.free.clear();
        self.size = 0;
        self.head = NIL;
        self.tail = NIL;
    }

    pub fn first_slot(&self) -> Option<u32> {
        self.order().first().copied()
    }

    /// Sizes the table for `n` mappings as `putMapEntries` does before copying them.
    pub fn presize(&mut self, n: usize) {
        if self.table.is_empty() && n > 0 {
            let t = ((n as f32 / self.load_factor) + 1.0) as usize;
            if t > self.threshold {
                self.threshold = table_size_for(t);
            }
        }
    }
}

// ------------------------------------------------------------------ JTree

/// TreeMap and TreeSet: entries kept sorted by the comparator (natural
/// order when 0), so iteration and navigation are the tree's.
#[derive(Debug, Clone, Default)]
pub struct JTree {
    pub entries: Vec<(Value, Value)>,
    pub comparator: ObjId,
    pub mod_count: u32,
    pub set: bool,
}

/// PriorityQueue: Java's binary heap, so `toString` and iteration show the
/// same array order.
#[derive(Debug, Clone, Default)]
pub struct JHeap {
    pub items: Vec<Value>,
    pub comparator: ObjId,
    pub mod_count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Joiner {
    pub delimiter: Vec<u16>,
    pub prefix: Vec<u16>,
    pub suffix: Vec<u16>,
    pub parts: Option<Vec<u16>>,
    pub empty: Option<Vec<u16>>,
}

#[derive(Debug, Clone)]
pub struct Regex {
    pub source: String,
    pub flags: i32,
    pub compiled: regex::Regex,
    /// Group names by index, for `group(String)`.
    pub names: Vec<Option<String>>,
    /// Matcher state: the input and the last match's group spans.
    pub input: Vec<u16>,
    pub text: String,
    pub groups: Vec<Option<(usize, usize)>>,
    pub from: usize,
    pub append_at: usize,
    pub pattern: ObjId,
}

// ------------------------------------------------------------------ iterators

#[derive(Debug, Clone)]
pub enum IterKind {
    /// A live list: index into it, fail-fast on its mod count.
    List,
    /// A snapshot of values (map views, trees, sets...).
    Snapshot(Vec<Value>),
}

#[derive(Debug, Clone)]
pub struct JIter {
    pub source: ObjId,
    pub kind: IterKind,
    pub pos: usize,
    pub last: Option<usize>,
    pub expected: u32,
    /// What `remove()` removes from the source: by index (lists) or by key.
    pub keys: Vec<Value>,
}

impl JIter {
    pub fn refs(&self, f: &mut dyn FnMut(Value)) {
        f(Value::Ref(self.source));
        if let IterKind::Snapshot(v) = &self.kind {
            v.iter().for_each(|x| f(*x));
        }
        self.keys.iter().for_each(|x| f(*x));
    }
}

fn mod_count_of(vm: &Vm, r: ObjId) -> u32 {
    match state(vm, r) {
        Some(State::List(l)) => l.mod_count,
        Some(State::Map(m)) => m.mod_count,
        Some(State::Tree(t)) => t.mod_count,
        Some(State::Heap(h)) => h.mod_count,
        _ => 0,
    }
}

fn new_iter(vm: &mut Vm, source: ObjId, kind: IterKind, keys: Vec<Value>) -> Res<Value> {
    let c = vm.load("java/util/Iterator$Native")?;
    let r = vm.new_instance(c)?;
    let expected = mod_count_of(vm, source);
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Iter(Box::new(JIter {
            source,
            kind,
            pos: 0,
            last: None,
            expected,
            keys,
        }))));
    }
    ret_ref(r)
}

fn cme(vm: &mut Vm) -> Exc {
    vm.throw_new("java/util/ConcurrentModificationException", None)
}

fn iter_has_next(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Iter(it)) = state(vm, t) else { return ret_bool(false) };
    let n = match &it.kind {
        IterKind::List => match state(vm, it.source) {
            Some(State::List(l)) => l.items.len(),
            _ => 0,
        },
        IterKind::Snapshot(v) => v.len(),
    };
    ret_bool(it.pos < n)
}

fn iter_next(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Iter(it)) = state(vm, t).cloned() else { return Err(vm.throw_new("java/util/NoSuchElementException", None)) };
    if mod_count_of(vm, it.source) != it.expected && it.source != 0 {
        return Err(cme(vm));
    }
    let v = match &it.kind {
        IterKind::List => match state(vm, it.source) {
            Some(State::List(l)) => l.items.get(it.pos).copied(),
            _ => None,
        },
        IterKind::Snapshot(v) => v.get(it.pos).copied(),
    };
    let Some(v) = v else { return Err(vm.throw_new("java/util/NoSuchElementException", None)) };
    if let Some(State::Iter(i)) = state_mut(vm, t) {
        i.last = Some(i.pos);
        i.pos += 1;
    }
    Ok(v)
}

fn iter_remove(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Iter(it)) = state(vm, t).cloned() else { return void() };
    let Some(last) = it.last else { return Err(vm.throw_new("java/lang/IllegalStateException", None)) };
    if mod_count_of(vm, it.source) != it.expected {
        return Err(cme(vm));
    }
    match it.kind {
        IterKind::List => {
            if let Some(State::List(l)) = state_mut(vm, it.source) {
                if l.immutable || l.fixed {
                    return Err(vm.throw_new("java/lang/UnsupportedOperationException", None));
                }
                l.items.remove(last);
                l.mod_count += 1;
            }
            if let Some(State::Iter(i)) = state_mut(vm, t) {
                i.pos = last;
                i.last = None;
            }
        }
        IterKind::Snapshot(_) => {
            let key = it.keys.get(last).copied().unwrap_or_default();
            remove_key_from(vm, it.source, key)?;
            if let Some(State::Iter(i)) = state_mut(vm, t) {
                i.last = None;
            }
        }
    }
    let mc = mod_count_of(vm, it.source);
    if let Some(State::Iter(i)) = state_mut(vm, t) {
        i.expected = mc;
    }
    void()
}

/// Removes a key from the collection an iterator came from (a map, a set,
/// a tree, a heap).
fn remove_key_from(vm: &mut Vm, source: ObjId, key: Value) -> Res<()> {
    match state(vm, source) {
        Some(State::Map(_)) => {
            map_remove(vm, source, key)?;
        }
        Some(State::Tree(_)) => {
            tree_remove(vm, source, key)?;
        }
        Some(State::Heap(_)) => {
            with_state(vm, source, |_, s| {
                if let State::Heap(h) = s
                    && let Some(i) = h.items.iter().position(|v| *v == key)
                {
                    h.items.remove(i);
                    h.mod_count += 1;
                }
                Ok(())
            })?;
        }
        _ => {
            vm.invoke_virtual(source, "remove", "(Ljava/lang/Object;)Z", &[key])?;
        }
    }
    Ok(())
}

/// A `ListIterator`'s extra moves, over a live list.
fn list_iter_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Iter(it)) = state(vm, t).cloned() else { return void() };
    match op {
        "hasPrevious" => ret_bool(it.pos > 0),
        "nextIndex" => Ok(Value::Int(it.pos as i32)),
        "previousIndex" => Ok(Value::Int(it.pos as i32 - 1)),
        "previous" => {
            if it.pos == 0 {
                return Err(vm.throw_new("java/util/NoSuchElementException", None));
            }
            let v = match state(vm, it.source) {
                Some(State::List(l)) => l.items.get(it.pos - 1).copied().unwrap_or_default(),
                _ => Value::NULL,
            };
            if let Some(State::Iter(i)) = state_mut(vm, t) {
                i.pos -= 1;
                i.last = Some(i.pos);
            }
            Ok(v)
        }
        "set" => {
            let Some(last) = it.last else { return Err(vm.throw_new("java/lang/IllegalStateException", None)) };
            if let Some(State::List(l)) = state_mut(vm, it.source) {
                if l.immutable {
                    return Err(vm.throw_new("java/lang/UnsupportedOperationException", None));
                }
                l.items[last] = arg(a, 1);
            }
            write_back(vm, it.source)?;
            void()
        }
        "add" => {
            if let Some(State::List(l)) = state_mut(vm, it.source) {
                if l.immutable || l.fixed {
                    return Err(vm.throw_new("java/lang/UnsupportedOperationException", None));
                }
                l.items.insert(it.pos, arg(a, 1));
                l.mod_count += 1;
            }
            let mc = mod_count_of(vm, it.source);
            if let Some(State::Iter(i)) = state_mut(vm, t) {
                i.pos += 1;
                i.last = None;
                i.expected = mc;
            }
            void()
        }
        _ => void(),
    }
}

// ------------------------------------------------------------------ helpers

/// The elements of any Iterable: fast for native collections, through
/// `iterator()` for everything else.
pub fn iterate(vm: &mut Vm, r: ObjId) -> Res<Vec<Value>> {
    if r == 0 {
        return Err(vm.npe(None));
    }
    if let Some(v) = native_elements(vm, r) {
        return Ok(v);
    }
    if vm.array(r).is_some() {
        return Ok(vm.array_values(r));
    }
    let it = vm.invoke_virtual(r, "iterator", "()Ljava/util/Iterator;", &[])?.r();
    let mut out = Vec::new();
    while vm.invoke_virtual(it, "hasNext", "()Z", &[])?.z() {
        out.push(vm.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?);
    }
    Ok(out)
}

/// The elements of a native collection (a list, a set, a map's view...),
/// in its iteration order; `None` for anything else.
fn native_elements(vm: &Vm, r: ObjId) -> Option<Vec<Value>> {
    let class = vm.class_of(r)?;
    if !matches!(vm.class(class).kind, crate::vm::ClassKind::Native(_)) {
        // A bytecode subclass may override iterator(): ask it.
        return None;
    }
    Some(match state(vm, r)? {
        State::List(l) => l.items.clone(),
        State::Map(m) if m.set => m.order().iter().map(|&s| m.entry(s).unwrap().key).collect(),
        State::Tree(t) if t.set => t.entries.iter().map(|e| e.0).collect(),
        State::Heap(h) => h.items.clone(),
        State::Generic(view) if view.len() == 2 => {
            let (map, kind) = (view[0].r(), view[1].i());
            return map_view_elements(vm, map, kind).map(|(v, _)| v);
        }
        _ => return None,
    })
}

/// A map's keys (0), values (1) or entries (2, as fresh Entry snapshots made
/// by the caller), with the keys alongside.
fn map_view_elements(vm: &Vm, map: ObjId, kind: i32) -> Option<(Vec<Value>, Vec<Value>)> {
    match state(vm, map)? {
        State::Map(m) => {
            let order = m.order();
            let keys: Vec<Value> = order.iter().map(|&s| m.entry(s).unwrap().key).collect();
            let values: Vec<Value> = order.iter().map(|&s| m.entry(s).unwrap().value).collect();
            Some((if kind == 1 { values } else { keys.clone() }, keys))
        }
        State::Tree(t) => {
            let keys: Vec<Value> = t.entries.iter().map(|e| e.0).collect();
            let values: Vec<Value> = t.entries.iter().map(|e| e.1).collect();
            Some((if kind == 1 { values } else { keys.clone() }, keys))
        }
        _ => None,
    }
}

/// `Objects.hashCode` of any value in a collection.
pub fn hash(vm: &mut Vm, v: Value) -> Res<i32> {
    super::lang::object_hash(vm, v.r())
}

pub fn equals(vm: &mut Vm, a: Value, b: Value) -> Res<bool> {
    super::lang::objects_equals(vm, a.r(), b.r())
}

fn uoe(vm: &mut Vm) -> Exc {
    vm.throw_new("java/lang/UnsupportedOperationException", None)
}

fn ioobe(vm: &mut Vm, i: i32, len: usize) -> Exc {
    let msg = format!("Index {i} out of bounds for length {len}");
    vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg))
}

/// A new native collection object of `class`.
pub fn new_collection(vm: &mut Vm, class: &str, s: State) -> Res<ObjId> {
    let c = vm.load(class)?;
    vm.ensure_init(c)?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(s));
    }
    Ok(r)
}

pub fn new_list(vm: &mut Vm, items: Vec<Value>) -> Res<ObjId> {
    new_collection(vm, "java/util/ArrayList", State::List(JList { items, ..Default::default() }))
}

pub fn new_immutable_list(vm: &mut Vm, items: Vec<Value>) -> Res<ObjId> {
    new_collection(vm, "java/util/ImmutableCollections$ListN", State::List(JList { items, immutable: true, ..Default::default() }))
}

/// `Stream.toList()`: unmodifiable, nulls allowed.
pub fn immutable_list_allowing_nulls(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    ret_ref(new_immutable_list(vm, items)?)
}

pub fn new_map(vm: &mut Vm, class: &str) -> Res<ObjId> {
    let mut m = JMap::new();
    m.linked = class == "java/util/LinkedHashMap";
    new_collection(vm, class, State::Map(Box::new(m)))
}

fn list_items(vm: &mut Vm, r: ObjId) -> Res<Vec<Value>> {
    match state(vm, r) {
        Some(State::List(l)) => Ok(l.items.clone()),
        _ => iterate(vm, r),
    }
}

fn with_list<T>(vm: &mut Vm, r: ObjId, write: bool, f: impl FnOnce(&mut Vm, &mut JList) -> Res<T>) -> Res<T> {
    let result = with_state(vm, r, |vm, s| {
        let State::List(l) = s else { return Err(Exc::Internal("not a list".into())) };
        if write && l.immutable {
            return Err(uoe(vm));
        }
        f(vm, l)
    })?;
    if write {
        write_back(vm, r)?;
    }
    Ok(result)
}

/// An `Arrays.asList` list writes its elements through to its array.
fn write_back(vm: &mut Vm, r: ObjId) -> Res<()> {
    if let Some(State::List(l)) = state(vm, r)
        && l.backing != 0
    {
        let (backing, items) = (l.backing, l.items.clone());
        for (i, v) in items.into_iter().enumerate() {
            vm.ref_array_set(backing, i, v.r());
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ lists

fn list_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let items = match a.get(1) {
        Some(Value::Ref(r)) => {
            if *r == 0 {
                return Err(vm.npe(None));
            }
            iterate(vm, *r)?
        }
        Some(Value::Int(n)) if *n < 0 => {
            let msg = format!("Illegal Capacity: {n}");
            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
        }
        _ => Vec::new(),
    };
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::List(JList { items, ..Default::default() })));
    }
    void()
}

fn list_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "size" => Ok(Value::Int(list_items(vm, t)?.len() as i32)),
        "isEmpty" => ret_bool(list_items(vm, t)?.is_empty()),
        "get" => {
            let i = arg(a, 1).i();
            let items = list_items(vm, t)?;
            match items.get(i as usize).filter(|_| i >= 0) {
                Some(v) => Ok(*v),
                None => Err(ioobe(vm, i, items.len())),
            }
        }
        "set" => {
            let (i, v) = (arg(a, 1).i(), arg(a, 2));
            with_list(vm, t, true, |vm, l| {
                let len = l.items.len();
                match l.items.get_mut(i as usize).filter(|_| i >= 0) {
                    Some(slot) => Ok(std::mem::replace(slot, v)),
                    None => Err(ioobe(vm, i, len)),
                }
            })
        }
        "add" => with_list(vm, t, true, |vm, l| {
            if l.fixed {
                return Err(uoe(vm));
            }
            l.items.push(arg(a, 1));
            l.mod_count += 1;
            ret_bool(true)
        }),
        "addAt" => {
            let (i, v) = (arg(a, 1).i(), arg(a, 2));
            with_list(vm, t, true, |vm, l| {
                if l.fixed {
                    return Err(uoe(vm));
                }
                if i < 0 || i as usize > l.items.len() {
                    return Err(ioobe(vm, i, l.items.len()));
                }
                l.items.insert(i as usize, v);
                l.mod_count += 1;
                void()
            })
        }
        "removeAt" => {
            let i = arg(a, 1).i();
            with_list(vm, t, true, |vm, l| {
                if l.fixed {
                    return Err(uoe(vm));
                }
                if i < 0 || i as usize >= l.items.len() {
                    return Err(ioobe(vm, i, l.items.len()));
                }
                l.mod_count += 1;
                Ok(l.items.remove(i as usize))
            })
        }
        "remove" => {
            let v = arg(a, 1);
            let items = list_items(vm, t)?;
            for (i, x) in items.iter().enumerate() {
                if equals(vm, v, *x)? {
                    with_list(vm, t, true, |vm, l| {
                        if l.fixed {
                            return Err(uoe(vm));
                        }
                        l.items.remove(i);
                        l.mod_count += 1;
                        Ok(())
                    })?;
                    return ret_bool(true);
                }
            }
            ret_bool(false)
        }
        "indexOf" | "contains" | "lastIndexOf" => {
            let v = arg(a, 1);
            let items = list_items(vm, t)?;
            let order: Vec<usize> = if op == "lastIndexOf" { (0..items.len()).rev().collect() } else { (0..items.len()).collect() };
            for i in order {
                if equals(vm, v, items[i])? {
                    return if op == "contains" { ret_bool(true) } else { Ok(Value::Int(i as i32)) };
                }
            }
            if op == "contains" { ret_bool(false) } else { Ok(Value::Int(-1)) }
        }
        "clear" => with_list(vm, t, true, |vm, l| {
            if l.fixed {
                return Err(uoe(vm));
            }
            l.items.clear();
            l.mod_count += 1;
            void()
        }),
        "addAll" => {
            let more = iterate(vm, arg(a, 1).r())?;
            with_list(vm, t, true, |vm, l| {
                if l.fixed {
                    return Err(uoe(vm));
                }
                let changed = !more.is_empty();
                l.items.extend(more);
                l.mod_count += 1;
                ret_bool(changed)
            })
        }
        "addAllAt" => {
            let i = arg(a, 1).i();
            let more = iterate(vm, arg(a, 2).r())?;
            with_list(vm, t, true, |vm, l| {
                if i < 0 || i as usize > l.items.len() {
                    return Err(ioobe(vm, i, l.items.len()));
                }
                let changed = !more.is_empty();
                l.items.splice(i as usize..i as usize, more);
                l.mod_count += 1;
                ret_bool(changed)
            })
        }
        "iterator" | "listIterator" => new_iter(vm, t, IterKind::List, Vec::new()),
        "listIteratorAt" => {
            let r = new_iter(vm, t, IterKind::List, Vec::new())?;
            if let Some(State::Iter(i)) = state_mut(vm, r.r()) {
                i.pos = arg(a, 1).i().max(0) as usize;
            }
            Ok(r)
        }
        "toArray" => {
            let items = list_items(vm, t)?;
            let refs: Vec<ObjId> = items.iter().map(|v| v.r()).collect();
            ret_ref(vm.new_ref_array("Ljava/lang/Object;", &refs)?)
        }
        "toArrayTyped" => {
            let items = list_items(vm, t)?;
            to_typed_array(vm, &items, arg(a, 1).r())
        }
        "subList" => {
            let (b, e) = (arg(a, 1).i(), arg(a, 2).i());
            let items = list_items(vm, t)?;
            if b < 0 || e as usize > items.len() || b > e {
                let msg = format!("fromIndex: {b}, toIndex: {e}, size: {}", items.len());
                return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
            }
            ret_ref(new_list(vm, items[b as usize..e as usize].to_vec())?)
        }
        "sort" => {
            let cmp = arg(a, 1).r();
            let mut items = list_items(vm, t)?;
            sort_values(vm, &mut items, cmp)?;
            with_list(vm, t, true, |_, l| {
                l.items = items;
                l.mod_count += 1;
                void()
            })
        }
        "replaceAll" => {
            let f = arg(a, 1).r();
            let items = list_items(vm, t)?;
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(vm.invoke_virtual(f, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?);
            }
            with_list(vm, t, true, |_, l| {
                l.items = out;
                void()
            })
        }
        "removeIf" => {
            let p = arg(a, 1).r();
            let items = list_items(vm, t)?;
            let mut keep = Vec::new();
            for v in &items {
                if !vm.invoke_virtual(p, "test", "(Ljava/lang/Object;)Z", &[*v])?.z() {
                    keep.push(*v);
                }
            }
            let changed = keep.len() != items.len();
            if changed {
                with_list(vm, t, true, |vm, l| {
                    if l.fixed {
                        return Err(uoe(vm));
                    }
                    l.items = keep;
                    l.mod_count += 1;
                    void()
                })?;
            }
            ret_bool(changed)
        }
        "forEach" => {
            let f = arg(a, 1).r();
            let expected = mod_count_of(vm, t);
            for v in list_items(vm, t)? {
                vm.invoke_virtual(f, "accept", "(Ljava/lang/Object;)V", &[v])?;
                if mod_count_of(vm, t) != expected {
                    return Err(cme(vm));
                }
            }
            void()
        }
        "hashCode" => {
            let mut h: i32 = 1;
            for v in list_items(vm, t)? {
                h = h.wrapping_mul(31).wrapping_add(hash(vm, v)?);
            }
            Ok(Value::Int(h))
        }
        "equals" => {
            let o = arg(a, 1).r();
            if o == t {
                return ret_bool(true);
            }
            let list = vm.load("java/util/List")?;
            if o == 0 || !vm.instance_of(o, list) {
                return ret_bool(false);
            }
            let (x, y) = (list_items(vm, t)?, iterate(vm, o)?);
            if x.len() != y.len() {
                return ret_bool(false);
            }
            for (p, q) in x.into_iter().zip(y) {
                if !equals(vm, p, q)? {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        "toString" => {
            let items = list_items(vm, t)?;
            collection_text(vm, t, &items)
        }
        "stream" => {
            let items = list_items(vm, t)?;
            super::stream::stream_of(vm, items)
        }
        "getFirst" | "getLast" | "removeFirst" | "removeLast" | "peekFirst" | "peekLast" | "pollFirst" | "pollLast" | "element" | "peek" | "poll" | "pop" | "remove0" => {
            let items = list_items(vm, t)?;
            let first = matches!(op, "getFirst" | "removeFirst" | "peekFirst" | "pollFirst" | "element" | "peek" | "poll" | "pop" | "remove0");
            let take = !matches!(op, "getFirst" | "getLast" | "peekFirst" | "peekLast" | "element" | "peek");
            let nullable = matches!(op, "peekFirst" | "peekLast" | "pollFirst" | "pollLast" | "peek" | "poll");
            if items.is_empty() {
                return if nullable { ret_ref(0) } else { Err(vm.throw_new("java/util/NoSuchElementException", None)) };
            }
            let i = if first { 0 } else { items.len() - 1 };
            let v = items[i];
            if take {
                with_list(vm, t, true, |vm, l| {
                    if l.fixed {
                        return Err(uoe(vm));
                    }
                    l.items.remove(i);
                    l.mod_count += 1;
                    void()
                })?;
            }
            Ok(v)
        }
        "addFirst" | "push" | "offerFirst" => {
            with_list(vm, t, true, |vm, l| {
                if l.fixed {
                    return Err(uoe(vm));
                }
                l.items.insert(0, arg(a, 1));
                l.mod_count += 1;
                void()
            })?;
            if op == "offerFirst" { ret_bool(true) } else { void() }
        }
        "addLast" | "offerLast" | "offer" => {
            if arg(a, 1).r() == 0 && vm.class_of(t).map(|c| vm.class_name(c) == "java/util/ArrayDeque").unwrap_or(false) {
                return Err(vm.npe(None));
            }
            with_list(vm, t, true, |vm, l| {
                if l.fixed {
                    return Err(uoe(vm));
                }
                l.items.push(arg(a, 1));
                l.mod_count += 1;
                void()
            })?;
            if op == "addLast" { void() } else { ret_bool(true) }
        }
        "reversed" => {
            let mut items = list_items(vm, t)?;
            items.reverse();
            ret_ref(new_list(vm, items)?)
        }
        "descendingIterator" => {
            let mut items = list_items(vm, t)?;
            items.reverse();
            new_iter(vm, 0, IterKind::Snapshot(items), Vec::new())
        }
        "removeFirstOccurrence" | "removeLastOccurrence" => {
            let v = arg(a, 1);
            let items = list_items(vm, t)?;
            let order: Vec<usize> = if op == "removeLastOccurrence" { (0..items.len()).rev().collect() } else { (0..items.len()).collect() };
            for i in order {
                if equals(vm, v, items[i])? {
                    with_list(vm, t, true, |_, l| {
                        l.items.remove(i);
                        l.mod_count += 1;
                        Ok(())
                    })?;
                    return ret_bool(true);
                }
            }
            ret_bool(false)
        }
        "containsAll" => {
            let items = list_items(vm, t)?;
            for v in iterate(vm, arg(a, 1).r())? {
                let mut found = false;
                for x in &items {
                    if equals(vm, v, *x)? {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        "removeAll" | "retainAll" => {
            let other = arg(a, 1).r();
            let items = list_items(vm, t)?;
            let mut keep = Vec::new();
            for v in &items {
                let contained = vm.invoke_virtual(other, "contains", "(Ljava/lang/Object;)Z", &[*v])?.z();
                if contained == (op == "retainAll") {
                    keep.push(*v);
                }
            }
            let changed = keep.len() != items.len();
            with_list(vm, t, true, |_, l| {
                l.items = keep;
                if changed {
                    l.mod_count += 1;
                }
                void()
            })?;
            ret_bool(changed)
        }
        _ => Err(Exc::Unsupported(format!("List.{op}"))),
    }
}

fn to_typed_array(vm: &mut Vm, items: &[Value], template: ObjId) -> Res<Value> {
    let Some(class) = vm.class_of(template) else { return Err(vm.npe(None)) };
    let len = vm.array(template).map_or(0, Array::len);
    let refs: Vec<ObjId> = items.iter().map(|v| v.r()).collect();
    if len >= refs.len() {
        for (i, r) in refs.iter().enumerate() {
            vm.ref_array_set(template, i, *r);
        }
        if len > refs.len() {
            vm.ref_array_set(template, refs.len(), 0);
        }
        return ret_ref(template);
    }
    let r = vm.new_array(class, refs.len() as i32)?;
    for (i, x) in refs.iter().enumerate() {
        vm.ref_array_set(r, i, *x);
    }
    ret_ref(r)
}

/// `AbstractCollection.toString`: `[a, b]`, with `(this Collection)` for itself.
fn collection_text(vm: &mut Vm, me: ObjId, items: &[Value]) -> Res<Value> {
    let mut out: Vec<u16> = vec!['[' as u16];
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.extend(", ".encode_utf16());
        }
        if v.r() == me && me != 0 {
            out.extend("(this Collection)".encode_utf16());
        } else {
            out.extend(vm.stringify(*v, "Ljava/lang/Object;")?);
        }
    }
    out.push(']' as u16);
    ret_ref(vm.new_string_units(out)?)
}

/// Sorts with a comparator (natural order for 0): a stable merge sort, as
/// `List.sort` and `Arrays.sort(Object[])` are.
pub fn sort_values(vm: &mut Vm, items: &mut [Value], comparator: ObjId) -> Res<()> {
    let mut err = None;
    let mut buf = items.to_vec();
    merge_sort(vm, &mut buf, comparator, &mut err);
    if let Some(e) = err {
        return Err(e);
    }
    items.copy_from_slice(&buf);
    Ok(())
}

fn merge_sort(vm: &mut Vm, v: &mut Vec<Value>, cmp: ObjId, err: &mut Option<Exc>) {
    if v.len() <= 1 || err.is_some() {
        return;
    }
    let mid = v.len() / 2;
    let mut right = v.split_off(mid);
    merge_sort(vm, v, cmp, err);
    merge_sort(vm, &mut right, cmp, err);
    let left = std::mem::take(v);
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        let c = if err.is_some() {
            0
        } else {
            match compare(vm, left[i], right[j], cmp) {
                Ok(c) => c,
                Err(e) => {
                    *err = Some(e);
                    0
                }
            }
        };
        if c <= 0 {
            v.push(left[i]);
            i += 1;
        } else {
            v.push(right[j]);
            j += 1;
        }
    }
    v.extend_from_slice(&left[i..]);
    v.extend_from_slice(&right[j..]);
}

/// `comparator.compare(a, b)`, or `a.compareTo(b)` for natural order.
pub fn compare(vm: &mut Vm, a: Value, b: Value, comparator: ObjId) -> Res<i32> {
    if comparator != 0 {
        return Ok(vm.invoke_virtual(comparator, "compare", "(Ljava/lang/Object;Ljava/lang/Object;)I", &[a, b])?.i());
    }
    let (x, y) = (a.r(), b.r());
    if x == 0 || y == 0 {
        return Err(vm.npe(None));
    }
    if let (Some(p), Some(q)) = (vm.string_units(x), vm.string_units(y)) {
        let c = p.iter().zip(q).find(|(a, b)| a != b).map(|(a, b)| *a as i32 - *b as i32).unwrap_or(p.len() as i32 - q.len() as i32);
        return Ok(c);
    }
    let comparable = vm.load("java/lang/Comparable")?;
    if !vm.instance_of(x, comparable) {
        let name = vm.class_of(x).map(|c| vm.class(c).java_name()).unwrap_or_default();
        let msg = format!("class {name} cannot be cast to class java.lang.Comparable ({name} is in unnamed module of loader 'app'; java.lang.Comparable is in module java.base of loader 'bootstrap')");
        return Err(vm.throw_new("java/lang/ClassCastException", Some(&msg)));
    }
    Ok(vm.invoke_virtual(x, "compareTo", "(Ljava/lang/Object;)I", &[b])?.i())
}

// ------------------------------------------------------------------ maps

fn map_hash(vm: &mut Vm, m: ObjId, key: Value) -> Res<i32> {
    let identity = matches!(state(vm, m), Some(State::Map(j)) if j.identity);
    let h = if identity { vm.identity_hash(key.r()) } else { hash(vm, key)? };
    Ok(JMap::spread(h))
}

fn map_find(vm: &mut Vm, m: ObjId, key: Value) -> Res<Option<u32>> {
    let h = map_hash(vm, m, key)?;
    let Some(State::Map(map)) = state(vm, m) else { return Ok(None) };
    let identity = map.identity;
    // The candidates of the key's bin, compared with equals() outside the borrow.
    let candidates: Vec<(u32, Value)> = if map.table.is_empty() {
        Vec::new()
    } else {
        let bin = (h as u32 as usize) & (map.table.len() - 1);
        map.table[bin].iter().filter_map(|&s| map.entry(s).filter(|e| e.hash == h).map(|e| (s, e.key))).collect()
    };
    for (slot, k) in candidates {
        let same = if identity { k.r() == key.r() } else { k.r() == key.r() || equals(vm, key, k)? };
        if same {
            return Ok(Some(slot));
        }
    }
    Ok(None)
}

pub fn map_get(vm: &mut Vm, m: ObjId, key: Value) -> Res<Option<Value>> {
    if let Some(State::Tree(_)) = state(vm, m) {
        return tree_get(vm, m, key);
    }
    let slot = map_find(vm, m, key)?;
    Ok(match slot {
        Some(s) => {
            let Some(State::Map(map)) = state_mut(vm, m) else { return Ok(None) };
            map.touch(s);
            map.entry(s).map(|e| e.value)
        }
        None => None,
    })
}

/// `put`: the previous value (`None` when the key was absent).
pub fn map_put(vm: &mut Vm, m: ObjId, key: Value, value: Value) -> Res<Option<Value>> {
    match state(vm, m) {
        Some(State::Tree(_)) => return tree_put(vm, m, key, value),
        Some(State::Map(map)) if map.immutable => return Err(uoe(vm)),
        _ => {}
    }
    if let Some(slot) = map_find(vm, m, key)? {
        let Some(State::Map(map)) = state_mut(vm, m) else { return Ok(None) };
        let old = map.set_value(slot, value);
        map.touch(slot);
        return Ok(Some(old));
    }
    let h = map_hash(vm, m, key)?;
    let check = match state_mut(vm, m) {
        Some(State::Map(map)) => {
            map.insert(h, key, value);
            map.check_eldest
        }
        _ => false,
    };
    if check {
        // LinkedHashMap.afterNodeInsertion: ask removeEldestEntry about the eldest.
        let eldest = match state(vm, m) {
            Some(State::Map(map)) => map.first_slot().and_then(|s| map.entry(s).map(|e| (e.key, e.value))),
            _ => None,
        };
        if let Some((k, v)) = eldest {
            let entry = new_entry(vm, k, v, m)?;
            let remove = vm.invoke_virtual(m, "removeEldestEntry", "(Ljava/util/Map$Entry;)Z", &[Value::Ref(entry)])?.z();
            if remove {
                map_remove(vm, m, k)?;
            }
        }
    }
    Ok(None)
}

/// `put` as `compute`, `computeIfAbsent` and `merge` do it: a new key goes
/// to the head of its bin.
fn map_put_computed(vm: &mut Vm, m: ObjId, key: Value, value: Value) -> Res<()> {
    let plain = matches!(state(vm, m), Some(State::Map(map)) if !map.immutable && !map.check_eldest);
    if !plain {
        map_put(vm, m, key, value)?;
        return Ok(());
    }
    if let Some(slot) = map_find(vm, m, key)? {
        if let Some(State::Map(map)) = state_mut(vm, m) {
            map.set_value(slot, value);
            map.touch(slot);
        }
        return Ok(());
    }
    let h = map_hash(vm, m, key)?;
    if let Some(State::Map(map)) = state_mut(vm, m) {
        map.insert_front(h, key, value);
    }
    Ok(())
}

pub fn map_remove(vm: &mut Vm, m: ObjId, key: Value) -> Res<Option<Value>> {
    match state(vm, m) {
        Some(State::Tree(_)) => return tree_remove(vm, m, key),
        Some(State::Map(map)) if map.immutable => return Err(uoe(vm)),
        _ => {}
    }
    let Some(slot) = map_find(vm, m, key)? else { return Ok(None) };
    let Some(State::Map(map)) = state_mut(vm, m) else { return Ok(None) };
    Ok(map.remove_slot(slot).map(|e| e.value))
}

pub fn new_entry(vm: &mut Vm, key: Value, value: Value, map: ObjId) -> Res<ObjId> {
    new_collection(vm, "java/util/HashMap$Node", State::Entry { key, value, map })
}

fn map_init(vm: &mut Vm, a: &[Value], linked: bool, identity: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let (cap, lf, access) = match &a[1..] {
        [Value::Int(c)] => (Some(*c), 0.75, false),
        [Value::Int(c), Value::Float(f)] => (Some(*c), *f, false),
        [Value::Int(c), Value::Float(f), Value::Int(acc)] => (Some(*c), *f, *acc != 0),
        _ => (None, 0.75, false),
    };
    if let Some(c) = cap
        && c < 0
    {
        let msg = format!("Illegal initial capacity: {c}");
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
    }
    let mut m = JMap::with_capacity(cap.map(|c| c as usize), lf);
    m.linked = linked;
    m.access_order = access;
    m.identity = identity;
    if identity {
        m = JMap::with_capacity(Some(64), 0.75);
        m.identity = true;
    }
    // A LinkedHashMap subclass that overrides removeEldestEntry is asked after each put.
    if linked
        && let Some(c) = vm.class_of(t)
        && !matches!(vm.class(c).kind, crate::vm::ClassKind::Native(_))
    {
        let nd = vm.nd("removeEldestEntry", "(Ljava/util/Map$Entry;)Z");
        if let Ok(sel) = vm.select(c, nd) {
            m.check_eldest = !matches!(vm.class(vm.method(sel).class).kind, crate::vm::ClassKind::Native(_));
        }
    }
    let source = match a.get(1) {
        Some(Value::Ref(r)) => Some(*r),
        _ => None,
    };
    if let Some(src) = source {
        if src == 0 {
            return Err(vm.npe(None));
        }
        let pairs = map_pairs(vm, src)?;
        m.presize(pairs.len());
        if let Some(o) = vm.obj_mut(t) {
            o.data = Data::Jdk(Box::new(State::Map(Box::new(m))));
        }
        for (k, v) in pairs {
            map_put(vm, t, k, v)?;
        }
        return void();
    }
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Map(Box::new(m))));
    }
    void()
}

/// A map's mappings in its iteration order (any Map).
pub fn map_pairs(vm: &mut Vm, m: ObjId) -> Res<Vec<(Value, Value)>> {
    match state(vm, m) {
        Some(State::Map(map)) => Ok(map.order().iter().map(|&s| map.entry(s).map(|e| (e.key, e.value)).unwrap()).collect()),
        Some(State::Tree(t)) => Ok(t.entries.clone()),
        _ => {
            let entries = vm.invoke_virtual(m, "entrySet", "()Ljava/util/Set;", &[])?.r();
            let mut out = Vec::new();
            for e in iterate(vm, entries)? {
                let k = vm.invoke_virtual(e.r(), "getKey", "()Ljava/lang/Object;", &[])?;
                let v = vm.invoke_virtual(e.r(), "getValue", "()Ljava/lang/Object;", &[])?;
                out.push((k, v));
            }
            Ok(out)
        }
    }
}

fn map_view(vm: &mut Vm, m: ObjId, kind: i32) -> Res<Value> {
    let class = match kind {
        0 => "java/util/HashMap$KeySet",
        1 => "java/util/HashMap$Values",
        _ => "java/util/HashMap$EntrySet",
    };
    ret_ref(new_collection(vm, class, State::Generic(vec![Value::Ref(m), Value::Int(kind)]))?)
}

fn map_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "size" => Ok(Value::Int(map_size(vm, t) as i32)),
        "isEmpty" => ret_bool(map_size(vm, t) == 0),
        "get" => Ok(map_get(vm, t, arg(a, 1))?.unwrap_or(Value::NULL)),
        "getOrDefault" => Ok(map_get(vm, t, arg(a, 1))?.unwrap_or(arg(a, 2))),
        "containsKey" => {
            if let Some(State::Tree(_)) = state(vm, t) {
                return ret_bool(tree_get(vm, t, arg(a, 1))?.is_some());
            }
            ret_bool(map_find(vm, t, arg(a, 1))?.is_some())
        }
        "containsValue" => {
            for (_, v) in map_pairs(vm, t)? {
                if equals(vm, arg(a, 1), v)? {
                    return ret_bool(true);
                }
            }
            ret_bool(false)
        }
        "put" => Ok(map_put(vm, t, arg(a, 1), arg(a, 2))?.unwrap_or(Value::NULL)),
        "putIfAbsent" => match map_get(vm, t, arg(a, 1))? {
            Some(v) if v.r() != 0 => Ok(v),
            _ => {
                map_put(vm, t, arg(a, 1), arg(a, 2))?;
                ret_ref(0)
            }
        },
        "remove" => Ok(map_remove(vm, t, arg(a, 1))?.unwrap_or(Value::NULL)),
        "removePair" => match map_get(vm, t, arg(a, 1))? {
            Some(v) if equals(vm, v, arg(a, 2))? => {
                map_remove(vm, t, arg(a, 1))?;
                ret_bool(true)
            }
            _ => ret_bool(false),
        },
        "replaceIf" => match map_get(vm, t, arg(a, 1))? {
            Some(v) if equals(vm, v, arg(a, 2))? => {
                map_put(vm, t, arg(a, 1), arg(a, 3))?;
                ret_bool(true)
            }
            _ => ret_bool(false),
        },
        "replace" => match map_get(vm, t, arg(a, 1))? {
            Some(old) => {
                map_put(vm, t, arg(a, 1), arg(a, 2))?;
                Ok(old)
            }
            None => ret_ref(0),
        },
        "clear" => {
            if let Some(State::Map(m)) = state(vm, t)
                && m.immutable
            {
                return Err(uoe(vm));
            }
            match state_mut(vm, t) {
                Some(State::Map(m)) => m.clear(),
                Some(State::Tree(tr)) => {
                    tr.entries.clear();
                    tr.mod_count += 1;
                }
                _ => {}
            }
            void()
        }
        "putAll" => {
            for (k, v) in map_pairs(vm, arg(a, 1).r())? {
                map_put(vm, t, k, v)?;
            }
            void()
        }
        "keySet" => map_view(vm, t, 0),
        "values" => map_view(vm, t, 1),
        "entrySet" => map_view(vm, t, 2),
        "forEach" => {
            let f = arg(a, 1).r();
            let expected = mod_count_of(vm, t);
            for (k, v) in map_pairs(vm, t)? {
                vm.invoke_virtual(f, "accept", "(Ljava/lang/Object;Ljava/lang/Object;)V", &[k, v])?;
                if mod_count_of(vm, t) != expected {
                    return Err(cme(vm));
                }
            }
            void()
        }
        "replaceAll" => {
            let f = arg(a, 1).r();
            for (k, v) in map_pairs(vm, t)? {
                let nv = vm.invoke_virtual(f, "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[k, v])?;
                map_put(vm, t, k, nv)?;
            }
            void()
        }
        "computeIfAbsent" => {
            let key = arg(a, 1);
            match map_get(vm, t, key)? {
                Some(v) if v.r() != 0 => Ok(v),
                _ => {
                    let expected = mod_count_of(vm, t);
                    let v = vm.invoke_virtual(arg(a, 2).r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[key])?;
                    if mod_count_of(vm, t) != expected {
                        return Err(cme(vm));
                    }
                    if v.r() != 0 {
                        map_put_computed(vm, t, key, v)?;
                    }
                    Ok(v)
                }
            }
        }
        "computeIfPresent" => {
            let key = arg(a, 1);
            match map_get(vm, t, key)? {
                Some(old) if old.r() != 0 => {
                    let v = vm.invoke_virtual(arg(a, 2).r(), "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[key, old])?;
                    if v.r() == 0 {
                        map_remove(vm, t, key)?;
                    } else {
                        map_put_computed(vm, t, key, v)?;
                    }
                    Ok(v)
                }
                _ => ret_ref(0),
            }
        }
        "compute" => {
            let key = arg(a, 1);
            let old = map_get(vm, t, key)?.unwrap_or(Value::NULL);
            let v = vm.invoke_virtual(arg(a, 2).r(), "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[key, old])?;
            if v.r() == 0 {
                map_remove(vm, t, key)?;
            } else {
                map_put_computed(vm, t, key, v)?;
            }
            Ok(v)
        }
        "merge" => {
            let (key, value, f) = (arg(a, 1), arg(a, 2), arg(a, 3).r());
            if value.r() == 0 || f == 0 {
                return Err(vm.npe(None));
            }
            let v = match map_get(vm, t, key)? {
                Some(old) if old.r() != 0 => vm.invoke_virtual(f, "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[old, value])?,
                _ => value,
            };
            if v.r() == 0 {
                map_remove(vm, t, key)?;
            } else {
                map_put_computed(vm, t, key, v)?;
            }
            Ok(v)
        }
        "hashCode" => {
            let mut h: i32 = 0;
            for (k, v) in map_pairs(vm, t)? {
                h = h.wrapping_add(hash(vm, k)? ^ hash(vm, v)?);
            }
            Ok(Value::Int(h))
        }
        "equals" => {
            let o = arg(a, 1).r();
            if o == t {
                return ret_bool(true);
            }
            let map_class = vm.load("java/util/Map")?;
            if o == 0 || !vm.instance_of(o, map_class) {
                return ret_bool(false);
            }
            let mine = map_pairs(vm, t)?;
            let size = vm.invoke_virtual(o, "size", "()I", &[])?.i();
            if size as usize != mine.len() {
                return ret_bool(false);
            }
            for (k, v) in mine {
                let other = vm.invoke_virtual(o, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[k])?;
                if !equals(vm, v, other)? {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        "toString" => {
            let pairs = map_pairs(vm, t)?;
            let mut out: Vec<u16> = vec!['{' as u16];
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.extend(", ".encode_utf16());
                }
                let ks = if k.r() == t { "(this Map)".encode_utf16().collect() } else { vm.stringify(*k, "Ljava/lang/Object;")? };
                out.extend(ks);
                out.push('=' as u16);
                let vs = if v.r() == t { "(this Map)".encode_utf16().collect() } else { vm.stringify(*v, "Ljava/lang/Object;")? };
                out.extend(vs);
            }
            out.push('}' as u16);
            ret_ref(vm.new_string_units(out)?)
        }
        "removeEldestEntry" => ret_bool(false),
        "firstEntry" | "lastEntry" | "pollFirstEntry" | "pollLastEntry" => {
            let pairs = map_pairs(vm, t)?;
            let first = op.contains("First") || op == "firstEntry";
            let Some(&(k, v)) = (if first { pairs.first() } else { pairs.last() }) else { return ret_ref(0) };
            if op.starts_with("poll") {
                map_remove(vm, t, k)?;
            }
            ret_ref(new_entry(vm, k, v, 0)?)
        }
        _ => Err(Exc::Unsupported(format!("Map.{op}"))),
    }
}

fn map_size(vm: &Vm, m: ObjId) -> usize {
    match state(vm, m) {
        Some(State::Map(map)) => map.size,
        Some(State::Tree(t)) => t.entries.len(),
        _ => 0,
    }
}

/// The map views (keySet, values, entrySet): live over their map.
fn view_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Generic(v)) = state(vm, t).cloned() else { return Err(Exc::Internal("not a map view".into())) };
    let (map, kind) = (v[0].r(), v[1].i());
    let elements = |vm: &mut Vm| -> Res<(Vec<Value>, Vec<Value>)> {
        let (items, keys) = map_view_elements(vm, map, kind).unwrap_or_default();
        if kind == 2 {
            let pairs = map_pairs(vm, map)?;
            let mut entries = Vec::new();
            for (k, val) in pairs {
                entries.push(Value::Ref(new_entry(vm, k, val, map)?));
            }
            return Ok((entries, keys));
        }
        Ok((items, keys))
    };
    match op {
        "size" => Ok(Value::Int(map_size(vm, map) as i32)),
        "isEmpty" => ret_bool(map_size(vm, map) == 0),
        "iterator" => {
            let (items, keys) = elements(vm)?;
            new_iter(vm, map, IterKind::Snapshot(items), keys)
        }
        "contains" => match kind {
            0 => {
                let r = vm.invoke_virtual(map, "containsKey", "(Ljava/lang/Object;)Z", &[arg(a, 1)])?;
                Ok(r)
            }
            1 => vm.invoke_virtual(map, "containsValue", "(Ljava/lang/Object;)Z", &[arg(a, 1)]),
            _ => {
                let e = arg(a, 1).r();
                if e == 0 {
                    return ret_bool(false);
                }
                let k = vm.invoke_virtual(e, "getKey", "()Ljava/lang/Object;", &[])?;
                let v = vm.invoke_virtual(e, "getValue", "()Ljava/lang/Object;", &[])?;
                let have = map_get(vm, map, k)?;
                ret_bool(match have {
                    Some(h) => equals(vm, h, v)?,
                    None => false,
                })
            }
        },
        "remove" => {
            if kind == 0 {
                let had = map_find(vm, map, arg(a, 1))?.is_some() || matches!(state(vm, map), Some(State::Tree(_)));
                map_remove(vm, map, arg(a, 1))?;
                ret_bool(had)
            } else {
                Err(uoe(vm))
            }
        }
        "clear" => {
            vm.invoke_virtual(map, "clear", "()V", &[])?;
            void()
        }
        "toString" => {
            let (items, _) = elements(vm)?;
            collection_text(vm, t, &items)
        }
        "toArray" => {
            let (items, _) = elements(vm)?;
            let refs: Vec<ObjId> = items.iter().map(|v| v.r()).collect();
            ret_ref(vm.new_ref_array("Ljava/lang/Object;", &refs)?)
        }
        "toArrayTyped" => {
            let (items, _) = elements(vm)?;
            to_typed_array(vm, &items, arg(a, 1).r())
        }
        "stream" => {
            let (items, _) = elements(vm)?;
            super::stream::stream_of(vm, items)
        }
        "forEach" => {
            let (items, _) = elements(vm)?;
            for v in items {
                vm.invoke_virtual(arg(a, 1).r(), "accept", "(Ljava/lang/Object;)V", &[v])?;
            }
            void()
        }
        "removeIf" => {
            let (items, keys) = elements(vm)?;
            let mut changed = false;
            for (v, k) in items.into_iter().zip(keys) {
                if vm.invoke_virtual(arg(a, 1).r(), "test", "(Ljava/lang/Object;)Z", &[v])?.z() {
                    map_remove(vm, map, k)?;
                    changed = true;
                }
            }
            ret_bool(changed)
        }
        "hashCode" => {
            let (items, _) = elements(vm)?;
            let mut h: i32 = 0;
            for v in items {
                h = h.wrapping_add(hash(vm, v)?);
            }
            Ok(Value::Int(h))
        }
        "equals" => set_equals(vm, t, arg(a, 1).r()),
        "containsAll" => {
            for v in iterate(vm, arg(a, 1).r())? {
                if !view_op(vm, &[Value::Ref(t), v], "contains")?.z() {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        _ => Err(Exc::Unsupported(format!("Map view {op}"))),
    }
}

fn set_equals(vm: &mut Vm, me: ObjId, o: ObjId) -> Res<Value> {
    if o == me {
        return ret_bool(true);
    }
    let set = vm.load("java/util/Set")?;
    if o == 0 || !vm.instance_of(o, set) {
        return ret_bool(false);
    }
    let mine = iterate(vm, me)?;
    let size = vm.invoke_virtual(o, "size", "()I", &[])?.i();
    if size as usize != mine.len() {
        return ret_bool(false);
    }
    for v in mine {
        if !vm.invoke_virtual(o, "contains", "(Ljava/lang/Object;)Z", &[v])?.z() {
            return ret_bool(false);
        }
    }
    ret_bool(true)
}

fn entry_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Entry { key, value, map }) = state(vm, t).cloned() else { return Err(Exc::Internal("not an entry".into())) };
    match op {
        "getKey" => Ok(key),
        "getValue" => Ok(value),
        "setValue" => {
            if map != 0 {
                map_put(vm, map, key, arg(a, 1))?;
            } else {
                return Err(uoe(vm));
            }
            if let Some(State::Entry { value: v, .. }) = state_mut(vm, t) {
                *v = arg(a, 1);
            }
            Ok(value)
        }
        "toString" => {
            let mut out = vm.stringify(key, "Ljava/lang/Object;")?;
            out.push('=' as u16);
            out.extend(vm.stringify(value, "Ljava/lang/Object;")?);
            ret_ref(vm.new_string_units(out)?)
        }
        "hashCode" => {
            let h = hash(vm, key)? ^ hash(vm, value)?;
            Ok(Value::Int(h))
        }
        "equals" => {
            let o = arg(a, 1).r();
            let entry_class = vm.load("java/util/Map$Entry")?;
            if o == 0 || !vm.instance_of(o, entry_class) {
                return ret_bool(false);
            }
            let k = vm.invoke_virtual(o, "getKey", "()Ljava/lang/Object;", &[])?;
            let v = vm.invoke_virtual(o, "getValue", "()Ljava/lang/Object;", &[])?;
            ret_bool(equals(vm, key, k)? && equals(vm, value, v)?)
        }
        _ => void(),
    }
}

// ------------------------------------------------------------------ sets

fn set_init(vm: &mut Vm, a: &[Value], linked: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let source = match a.get(1) {
        Some(Value::Ref(r)) => Some(*r),
        _ => None,
    };
    let (cap, lf) = match (&a[1..], source) {
        (_, Some(src)) => {
            if src == 0 {
                return Err(vm.npe(None));
            }
            let n = iterate(vm, src)?.len();
            if linked { (Some((2 * n).max(11)), 0.75) } else { (Some(((n as f32 / 0.75) as usize + 1).max(16)), 0.75) }
        }
        ([Value::Int(c)], _) => (Some(*c as usize), 0.75),
        ([Value::Int(c), Value::Float(f)], _) => (Some(*c as usize), *f),
        _ => (None, 0.75),
    };
    let mut m = JMap::with_capacity(cap, lf);
    m.linked = linked;
    m.set = true;
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Map(Box::new(m))));
    }
    if let Some(src) = source {
        for v in iterate(vm, src)? {
            map_put(vm, t, v, Value::Int(1))?;
        }
    }
    void()
}

fn set_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    if let Some(State::Tree(_)) = state(vm, t) {
        return tree_set_op(vm, a, op);
    }
    match op {
        "add" => {
            if let Some(State::Map(m)) = state(vm, t)
                && m.immutable
            {
                return Err(uoe(vm));
            }
            ret_bool(map_put(vm, t, arg(a, 1), Value::Int(1))?.is_none())
        }
        "remove" => {
            if let Some(State::Map(m)) = state(vm, t)
                && m.immutable
            {
                return Err(uoe(vm));
            }
            ret_bool(map_remove(vm, t, arg(a, 1))?.is_some())
        }
        "contains" => ret_bool(map_find(vm, t, arg(a, 1))?.is_some()),
        "size" => Ok(Value::Int(map_size(vm, t) as i32)),
        "isEmpty" => ret_bool(map_size(vm, t) == 0),
        "clear" => map_op(vm, a, "clear"),
        "iterator" => {
            let items = native_elements(vm, t).unwrap_or_default();
            new_iter(vm, t, IterKind::Snapshot(items.clone()), items)
        }
        "addAll" => {
            let mut changed = false;
            for v in iterate(vm, arg(a, 1).r())? {
                changed |= map_put(vm, t, v, Value::Int(1))?.is_none();
            }
            ret_bool(changed)
        }
        "removeAll" => {
            let mut changed = false;
            for v in iterate(vm, arg(a, 1).r())? {
                changed |= map_remove(vm, t, v)?.is_some();
            }
            ret_bool(changed)
        }
        "retainAll" | "removeIf" => {
            let other = arg(a, 1).r();
            let mut changed = false;
            for v in native_elements(vm, t).unwrap_or_default() {
                let drop = if op == "retainAll" {
                    !vm.invoke_virtual(other, "contains", "(Ljava/lang/Object;)Z", &[v])?.z()
                } else {
                    vm.invoke_virtual(other, "test", "(Ljava/lang/Object;)Z", &[v])?.z()
                };
                if drop {
                    map_remove(vm, t, v)?;
                    changed = true;
                }
            }
            ret_bool(changed)
        }
        "containsAll" => {
            for v in iterate(vm, arg(a, 1).r())? {
                if map_find(vm, t, v)?.is_none() {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        "toString" => {
            let items = native_elements(vm, t).unwrap_or_default();
            collection_text(vm, t, &items)
        }
        "hashCode" => {
            let mut h: i32 = 0;
            for v in native_elements(vm, t).unwrap_or_default() {
                h = h.wrapping_add(hash(vm, v)?);
            }
            Ok(Value::Int(h))
        }
        "equals" => set_equals(vm, t, arg(a, 1).r()),
        "toArray" => {
            let refs: Vec<ObjId> = native_elements(vm, t).unwrap_or_default().iter().map(|v| v.r()).collect();
            ret_ref(vm.new_ref_array("Ljava/lang/Object;", &refs)?)
        }
        "toArrayTyped" => {
            let items = native_elements(vm, t).unwrap_or_default();
            to_typed_array(vm, &items, arg(a, 1).r())
        }
        "stream" => {
            let items = native_elements(vm, t).unwrap_or_default();
            super::stream::stream_of(vm, items)
        }
        "forEach" => {
            for v in native_elements(vm, t).unwrap_or_default() {
                vm.invoke_virtual(arg(a, 1).r(), "accept", "(Ljava/lang/Object;)V", &[v])?;
            }
            void()
        }
        "getFirst" | "getLast" | "removeFirst" | "removeLast" => {
            let items = native_elements(vm, t).unwrap_or_default();
            let Some(&v) = (if op.ends_with("First") { items.first() } else { items.last() }) else {
                return Err(vm.throw_new("java/util/NoSuchElementException", None));
            };
            if op.starts_with("remove") {
                map_remove(vm, t, v)?;
            }
            Ok(v)
        }
        "reversed" => {
            let mut items = native_elements(vm, t).unwrap_or_default();
            items.reverse();
            let r = new_collection(vm, "java/util/LinkedHashSet", State::Map(Box::new({
                let mut m = JMap::new();
                m.linked = true;
                m.set = true;
                m
            })))?;
            for v in items {
                map_put(vm, r, v, Value::Int(1))?;
            }
            ret_ref(r)
        }
        _ => Err(Exc::Unsupported(format!("Set.{op}"))),
    }
}

// ------------------------------------------------------------------ trees

fn tree_cmp(vm: &Vm, t: ObjId) -> ObjId {
    match state(vm, t) {
        Some(State::Tree(tr)) => tr.comparator,
        _ => 0,
    }
}

fn tree_entries(vm: &Vm, t: ObjId) -> Vec<(Value, Value)> {
    match state(vm, t) {
        Some(State::Tree(tr)) => tr.entries.clone(),
        _ => Vec::new(),
    }
}

/// The position of `key` among the sorted entries: `Ok(found)` or
/// `Err(insertion point)`.
fn tree_search(vm: &mut Vm, t: ObjId, key: Value) -> Res<Result<usize, usize>> {
    let cmp = tree_cmp(vm, t);
    let entries = tree_entries(vm, t);
    if key.r() == 0 && cmp == 0 {
        return Err(vm.npe(None));
    }
    let (mut lo, mut hi) = (0usize, entries.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let c = compare(vm, entries[mid].0, key, cmp)?;
        if c < 0 {
            lo = mid + 1;
        } else if c > 0 {
            hi = mid;
        } else {
            return Ok(Ok(mid));
        }
    }
    Ok(Err(lo))
}

fn tree_get(vm: &mut Vm, t: ObjId, key: Value) -> Res<Option<Value>> {
    Ok(match tree_search(vm, t, key)? {
        Ok(i) => Some(tree_entries(vm, t)[i].1),
        Err(_) => None,
    })
}

fn tree_put(vm: &mut Vm, t: ObjId, key: Value, value: Value) -> Res<Option<Value>> {
    if tree_entries(vm, t).is_empty() {
        // TreeMap.put type-checks its first key against itself.
        let cmp = tree_cmp(vm, t);
        compare(vm, key, key, cmp)?;
    }
    let found = tree_search(vm, t, key)?;
    let Some(State::Tree(tr)) = state_mut(vm, t) else { return Ok(None) };
    Ok(match found {
        Ok(i) => Some(std::mem::replace(&mut tr.entries[i].1, value)),
        Err(i) => {
            tr.entries.insert(i, (key, value));
            tr.mod_count += 1;
            None
        }
    })
}

fn tree_remove(vm: &mut Vm, t: ObjId, key: Value) -> Res<Option<Value>> {
    let found = tree_search(vm, t, key)?;
    let Some(State::Tree(tr)) = state_mut(vm, t) else { return Ok(None) };
    Ok(match found {
        Ok(i) => {
            tr.mod_count += 1;
            Some(tr.entries.remove(i).1)
        }
        Err(_) => None,
    })
}

fn tree_init(vm: &mut Vm, a: &[Value], set: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let comparator_class = vm.load("java/util/Comparator")?;
    let mut tree = JTree { set, ..Default::default() };
    let source = match a.get(1) {
        Some(Value::Ref(r)) if *r != 0 && vm.instance_of(*r, comparator_class) => {
            tree.comparator = *r;
            None
        }
        Some(Value::Ref(r)) if *r != 0 => Some(*r),
        _ => None,
    };
    if let Some(src) = source
        && let Some(State::Tree(other)) = state(vm, src)
    {
        tree.comparator = other.comparator;
    }
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Tree(Box::new(tree))));
    }
    if let Some(src) = source {
        if set {
            for v in iterate(vm, src)? {
                tree_put(vm, t, v, Value::Int(1))?;
            }
        } else {
            for (k, v) in map_pairs(vm, src)? {
                tree_put(vm, t, k, v)?;
            }
        }
    }
    void()
}

/// TreeMap's navigation: the entry at or around a key.
fn tree_nav(vm: &mut Vm, t: ObjId, key: Value, op: &str) -> Res<Option<(Value, Value)>> {
    let entries = tree_entries(vm, t);
    let pos = tree_search(vm, t, key)?;
    let i: Option<usize> = match (op, pos) {
        ("floor", Ok(i)) | ("ceiling", Ok(i)) => Some(i),
        ("floor", Err(i)) | ("lower", Err(i)) | ("lower", Ok(i)) => i.checked_sub(1),
        ("ceiling", Err(i)) | ("higher", Err(i)) => Some(i),
        ("higher", Ok(i)) => Some(i + 1),
        _ => None,
    };
    Ok(i.and_then(|i| entries.get(i).copied()))
}

fn new_tree_like(vm: &mut Vm, t: ObjId, entries: Vec<(Value, Value)>, reverse: bool) -> Res<ObjId> {
    let (set, cmp) = match state(vm, t) {
        Some(State::Tree(tr)) => (tr.set, tr.comparator),
        _ => (false, 0),
    };
    let cmp = if reverse { native_comparator(vm, Cmp::Reverse(cmp))? } else { cmp };
    let mut entries = entries;
    if reverse {
        entries.reverse();
    }
    let class = if set { "java/util/TreeSet" } else { "java/util/TreeMap" };
    new_collection(vm, class, State::Tree(Box::new(JTree { entries, comparator: cmp, mod_count: 0, set })))
}

fn tree_range(vm: &mut Vm, t: ObjId, from: Option<(Value, bool)>, to: Option<(Value, bool)>) -> Res<Vec<(Value, Value)>> {
    let cmp = tree_cmp(vm, t);
    let mut out = Vec::new();
    for (k, v) in tree_entries(vm, t) {
        if let Some((f, inclusive)) = from {
            let c = compare(vm, k, f, cmp)?;
            if c < 0 || (c == 0 && !inclusive) {
                continue;
            }
        }
        if let Some((e, inclusive)) = to {
            let c = compare(vm, k, e, cmp)?;
            if c > 0 || (c == 0 && !inclusive) {
                continue;
            }
        }
        out.push((k, v));
    }
    Ok(out)
}

fn tree_map_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let key_of = |e: Option<(Value, Value)>| e.map_or(Value::NULL, |e| e.0);
    match op {
        "firstKey" | "lastKey" => {
            let entries = tree_entries(vm, t);
            let e = if op == "firstKey" { entries.first() } else { entries.last() };
            match e {
                Some(e) => Ok(e.0),
                None => Err(vm.throw_new("java/util/NoSuchElementException", None)),
            }
        }
        "floorKey" | "ceilingKey" | "higherKey" | "lowerKey" => {
            let e = tree_nav(vm, t, arg(a, 1), &op[..op.len() - 3])?;
            Ok(key_of(e))
        }
        "floorEntry" | "ceilingEntry" | "higherEntry" | "lowerEntry" => {
            match tree_nav(vm, t, arg(a, 1), &op[..op.len() - 5])? {
                Some((k, v)) => ret_ref(new_entry(vm, k, v, 0)?),
                None => ret_ref(0),
            }
        }
        "headMap" | "headMapIncl" => {
            let incl = op == "headMapIncl" && arg(a, 2).z();
            let r = tree_range(vm, t, None, Some((arg(a, 1), incl)))?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "tailMap" | "tailMapIncl" => {
            let incl = op == "tailMap" || arg(a, 2).z();
            let r = tree_range(vm, t, Some((arg(a, 1), incl)), None)?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "subMap" => {
            let r = tree_range(vm, t, Some((arg(a, 1), true)), Some((arg(a, 2), false)))?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "subMapIncl" => {
            let r = tree_range(vm, t, Some((arg(a, 1), arg(a, 2).z())), Some((arg(a, 3), arg(a, 4).z())))?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "descendingMap" | "descendingSet" => {
            let entries = tree_entries(vm, t);
            ret_ref(new_tree_like(vm, t, entries, true)?)
        }
        "navigableKeySet" | "descendingKeySet" => {
            let mut keys: Vec<(Value, Value)> = tree_entries(vm, t).into_iter().map(|e| (e.0, Value::Int(1))).collect();
            let cmp = tree_cmp(vm, t);
            let cmp = if op == "descendingKeySet" {
                keys.reverse();
                native_comparator(vm, Cmp::Reverse(cmp))?
            } else {
                cmp
            };
            ret_ref(new_collection(vm, "java/util/TreeSet", State::Tree(Box::new(JTree { entries: keys, comparator: cmp, mod_count: 0, set: true })))?)
        }
        "comparator" => ret_ref(tree_cmp(vm, t)),
        _ => map_op(vm, a, op),
    }
}

fn tree_set_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let keys = |vm: &Vm| -> Vec<Value> { tree_entries(vm, t).into_iter().map(|e| e.0).collect() };
    match op {
        "add" => ret_bool(tree_put(vm, t, arg(a, 1), Value::Int(1))?.is_none()),
        "remove" => ret_bool(tree_remove(vm, t, arg(a, 1))?.is_some()),
        "contains" => ret_bool(tree_get(vm, t, arg(a, 1))?.is_some()),
        "size" => Ok(Value::Int(tree_entries(vm, t).len() as i32)),
        "isEmpty" => ret_bool(tree_entries(vm, t).is_empty()),
        "iterator" => {
            let k = keys(vm);
            new_iter(vm, t, IterKind::Snapshot(k.clone()), k)
        }
        "descendingIterator" => {
            let mut k = keys(vm);
            k.reverse();
            new_iter(vm, t, IterKind::Snapshot(k.clone()), k)
        }
        "first" | "last" | "pollFirst" | "pollLast" => {
            let k = keys(vm);
            let v = if op.ends_with("irst") { k.first() } else { k.last() }.copied();
            match v {
                Some(v) => {
                    if op.starts_with("poll") {
                        tree_remove(vm, t, v)?;
                    }
                    Ok(v)
                }
                None if op.starts_with("poll") => ret_ref(0),
                None => Err(vm.throw_new("java/util/NoSuchElementException", None)),
            }
        }
        "floor" | "ceiling" | "higher" | "lower" => Ok(tree_nav(vm, t, arg(a, 1), op)?.map_or(Value::NULL, |e| e.0)),
        "headSet" | "headSetIncl" => {
            let incl = op == "headSetIncl" && arg(a, 2).z();
            let r = tree_range(vm, t, None, Some((arg(a, 1), incl)))?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "tailSet" | "tailSetIncl" => {
            let incl = op == "tailSet" || arg(a, 2).z();
            let r = tree_range(vm, t, Some((arg(a, 1), incl)), None)?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "subSet" => {
            let r = tree_range(vm, t, Some((arg(a, 1), true)), Some((arg(a, 2), false)))?;
            ret_ref(new_tree_like(vm, t, r, false)?)
        }
        "descendingSet" => {
            let entries = tree_entries(vm, t);
            ret_ref(new_tree_like(vm, t, entries, true)?)
        }
        "toString" => {
            let k = keys(vm);
            collection_text(vm, t, &k)
        }
        "comparator" => ret_ref(tree_cmp(vm, t)),
        _ => {
            // Everything else (addAll, stream, toArray...) works on the keys.
            let k = keys(vm);
            match op {
                "addAll" => {
                    let mut changed = false;
                    for v in iterate(vm, arg(a, 1).r())? {
                        changed |= tree_put(vm, t, v, Value::Int(1))?.is_none();
                    }
                    ret_bool(changed)
                }
                "stream" => super::stream::stream_of(vm, k),
                "toArray" => {
                    let refs: Vec<ObjId> = k.iter().map(|v| v.r()).collect();
                    ret_ref(vm.new_ref_array("Ljava/lang/Object;", &refs)?)
                }
                "forEach" => {
                    for v in k {
                        vm.invoke_virtual(arg(a, 1).r(), "accept", "(Ljava/lang/Object;)V", &[v])?;
                    }
                    void()
                }
                "hashCode" => {
                    let mut h: i32 = 0;
                    for v in k {
                        h = h.wrapping_add(hash(vm, v)?);
                    }
                    Ok(Value::Int(h))
                }
                "equals" => set_equals(vm, t, arg(a, 1).r()),
                "clear" => {
                    if let Some(State::Tree(tr)) = state_mut(vm, t) {
                        tr.entries.clear();
                        tr.mod_count += 1;
                    }
                    void()
                }
                "removeIf" => {
                    let mut changed = false;
                    for v in k {
                        if vm.invoke_virtual(arg(a, 1).r(), "test", "(Ljava/lang/Object;)Z", &[v])?.z() {
                            tree_remove(vm, t, v)?;
                            changed = true;
                        }
                    }
                    ret_bool(changed)
                }
                _ => Err(Exc::Unsupported(format!("TreeSet.{op}"))),
            }
        }
    }
}

// ------------------------------------------------------------------ enums

fn ordinal_comparator(vm: &mut Vm, enum_class: ObjId) -> Res<ObjId> {
    native_comparator(vm, Cmp::Ordinal(enum_class))
}

/// The enum class whose constants a value belongs to (a constant with a body
/// is an anonymous subclass).
fn enum_class_of(vm: &mut Vm, e: ObjId) -> Res<ObjId> {
    let Some(mut c) = vm.class_of(e) else { return Err(vm.npe(None)) };
    if let Some(s) = vm.class(c).super_class
        && vm.class_name(s) != "java/lang/Enum"
        && vm.class(s).access & 0x4000 != 0
    {
        c = s;
    }
    vm.mirror(c)
}

fn enum_map_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let src = arg(a, 1).r();
    if src == 0 {
        return Err(vm.npe(None));
    }
    let (mirror, entries) = if vm.mirror_class(src).is_some() {
        (src, Vec::new())
    } else {
        let cmp = tree_cmp(vm, src);
        let mirror = match state(vm, cmp) {
            Some(State::Generic(v)) if v.len() == 2 => v[1].r(),
            _ => 0,
        };
        (mirror, tree_entries(vm, src))
    };
    let comparator = ordinal_comparator(vm, mirror)?;
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Tree(Box::new(JTree { entries, comparator, mod_count: 0, set: false }))));
    }
    void()
}

fn new_enum_set(vm: &mut Vm, mirror: ObjId, mut items: Vec<Value>) -> Res<Value> {
    let comparator = ordinal_comparator(vm, mirror)?;
    items.sort_by_key(|v| super::lang::enum_parts(vm, v.r()).1);
    items.dedup_by_key(|v| v.r());
    let entries = items.into_iter().map(|v| (v, Value::Int(1))).collect();
    ret_ref(new_collection(vm, "java/util/RegularEnumSet", State::Tree(Box::new(JTree { entries, comparator, mod_count: 0, set: true })))?)
}

fn enum_set(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "noneOf" | "allOf" => {
            let mirror = arg(a, 0).r();
            let Some(c) = vm.mirror_class(mirror) else { return Err(vm.npe(None)) };
            if vm.class(c).access & 0x4000 == 0 {
                let msg = format!("{} not an enum", vm.class(c).java_name());
                return Err(vm.throw_new("java/lang/ClassCastException", Some(&msg)));
            }
            let items = if op == "allOf" { super::lang::enum_constants(vm, c)?.into_iter().map(Value::Ref).collect() } else { Vec::new() };
            new_enum_set(vm, mirror, items)
        }
        "of" | "ofArray" => {
            let mut items: Vec<Value> = if op == "ofArray" {
                let mut v = vec![arg(a, 0)];
                v.extend(vm.array_values(arg(a, 1).r()));
                v
            } else {
                a.to_vec()
            };
            if items.iter().any(|v| v.r() == 0) {
                return Err(vm.npe(None));
            }
            let mirror = enum_class_of(vm, items[0].r())?;
            items.retain(|v| v.r() != 0);
            new_enum_set(vm, mirror, items)
        }
        "range" => {
            let (from, to) = (arg(a, 0).r(), arg(a, 1).r());
            if from == 0 || to == 0 {
                return Err(vm.npe(None));
            }
            let mirror = enum_class_of(vm, from)?;
            let (lo, hi) = (super::lang::enum_parts(vm, from).1, super::lang::enum_parts(vm, to).1);
            if lo > hi {
                let msg = format!("{} > {}", super::lang::enum_name(vm, from).unwrap_or_default(), super::lang::enum_name(vm, to).unwrap_or_default());
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
            }
            let c = vm.mirror_class(mirror).unwrap_or(0);
            let items = super::lang::enum_constants(vm, c)?.into_iter().filter(|&e| (lo..=hi).contains(&super::lang::enum_parts(vm, e).1)).map(Value::Ref).collect();
            new_enum_set(vm, mirror, items)
        }
        "copyOf" | "complementOf" => {
            let src = arg(a, 0).r();
            if src == 0 {
                return Err(vm.npe(None));
            }
            let items = iterate(vm, src)?;
            let mirror = match state(vm, src) {
                Some(State::Tree(t)) => match state(vm, t.comparator) {
                    Some(State::Generic(v)) if v.len() == 2 => v[1].r(),
                    _ => 0,
                },
                _ => match items.first() {
                    Some(v) => enum_class_of(vm, v.r())?,
                    None => return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Collection is empty"))),
                },
            };
            if op == "copyOf" {
                return new_enum_set(vm, mirror, items);
            }
            let c = vm.mirror_class(mirror).unwrap_or(0);
            let present: Vec<ObjId> = items.iter().map(|v| v.r()).collect();
            let rest = super::lang::enum_constants(vm, c)?.into_iter().filter(|e| !present.contains(e)).map(Value::Ref).collect();
            new_enum_set(vm, mirror, rest)
        }
        _ => Err(Exc::Unsupported(format!("EnumSet.{op}"))),
    }
}

// ------------------------------------------------------------------ heap

fn heap_sift_up(vm: &mut Vm, items: &mut [Value], mut k: usize, x: Value, cmp: ObjId) -> Res<()> {
    while k > 0 {
        let parent = (k - 1) >> 1;
        let e = items[parent];
        if compare(vm, x, e, cmp)? >= 0 {
            break;
        }
        items[k] = e;
        k = parent;
    }
    items[k] = x;
    Ok(())
}

fn heap_sift_down(vm: &mut Vm, items: &mut [Value], mut k: usize, x: Value, n: usize, cmp: ObjId) -> Res<()> {
    let half = n >> 1;
    while k < half {
        let mut child = (k << 1) + 1;
        let mut c = items[child];
        let right = child + 1;
        if right < n && compare(vm, c, items[right], cmp)? > 0 {
            child = right;
            c = items[child];
        }
        if compare(vm, x, c, cmp)? <= 0 {
            break;
        }
        items[k] = c;
        k = child;
    }
    items[k] = x;
    Ok(())
}

fn heap_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let (mut items, cmp) = match state(vm, t) {
        Some(State::Heap(h)) => (h.items.clone(), h.comparator),
        _ => (Vec::new(), 0),
    };
    let result = match op {
        "add" | "offer" => {
            let x = arg(a, 1);
            if x.r() == 0 {
                return Err(vm.npe(None));
            }
            items.push(x);
            let k = items.len() - 1;
            if k == 0 {
                compare(vm, x, x, cmp)?;
            }
            heap_sift_up(vm, &mut items, k, x, cmp)?;
            Value::Int(1)
        }
        "poll" | "remove0" => {
            if items.is_empty() {
                if op == "poll" {
                    return ret_ref(0);
                }
                return Err(vm.throw_new("java/util/NoSuchElementException", None));
            }
            let result = items[0];
            let x = items.pop().unwrap();
            let n = items.len();
            if n > 0 {
                heap_sift_down(vm, &mut items, 0, x, n, cmp)?;
            }
            result
        }
        "peek" | "element" => {
            return match items.first() {
                Some(v) => Ok(*v),
                None if op == "peek" => ret_ref(0),
                None => Err(vm.throw_new("java/util/NoSuchElementException", None)),
            };
        }
        "remove" => {
            let x = arg(a, 1);
            let mut at = None;
            for (i, v) in items.iter().enumerate() {
                if equals(vm, x, *v)? {
                    at = Some(i);
                    break;
                }
            }
            let Some(i) = at else { return ret_bool(false) };
            let s = items.len() - 1;
            if s == i {
                items.pop();
            } else {
                let moved = items.pop().unwrap();
                heap_sift_down(vm, &mut items, i, moved, s, cmp)?;
                if items[i] == moved {
                    heap_sift_up(vm, &mut items, i, moved, cmp)?;
                }
            }
            Value::Int(1)
        }
        "size" => return Ok(Value::Int(items.len() as i32)),
        "isEmpty" => return ret_bool(items.is_empty()),
        "clear" => {
            items.clear();
            Value::Top
        }
        "toString" => return collection_text(vm, t, &items),
        "iterator" => return new_iter(vm, t, IterKind::Snapshot(items.clone()), items),
        "contains" => {
            for v in &items {
                if equals(vm, arg(a, 1), *v)? {
                    return ret_bool(true);
                }
            }
            return ret_bool(false);
        }
        "toArray" => {
            let refs: Vec<ObjId> = items.iter().map(|v| v.r()).collect();
            return ret_ref(vm.new_ref_array("Ljava/lang/Object;", &refs)?);
        }
        "comparator" => return ret_ref(cmp),
        "stream" => return super::stream::stream_of(vm, items),
        "addAll" => {
            for v in iterate(vm, arg(a, 1).r())? {
                heap_op(vm, &[Value::Ref(t), v], "add")?;
            }
            return ret_bool(true);
        }
        _ => return Err(Exc::Unsupported(format!("PriorityQueue.{op}"))),
    };
    if let Some(State::Heap(h)) = state_mut(vm, t) {
        h.items = items;
        h.mod_count += 1;
    }
    Ok(match op {
        "add" | "offer" | "remove" => Value::Int(1),
        "clear" => Value::Top,
        _ => result,
    })
}

fn heap_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let comparator_class = vm.load("java/util/Comparator")?;
    let mut comparator = 0;
    let mut source = None;
    for v in &a[1..] {
        if let Value::Ref(r) = v
            && *r != 0
        {
            if vm.instance_of(*r, comparator_class) {
                comparator = *r;
            } else {
                source = Some(*r);
            }
        }
    }
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Heap(Box::new(JHeap { items: Vec::new(), comparator, mod_count: 0 }))));
    }
    if let Some(src) = source {
        for v in iterate(vm, src)? {
            heap_op(vm, &[Value::Ref(t), v], "add")?;
        }
    }
    void()
}

// ------------------------------------------------------------------ comparators

/// The JDK's comparators: natural, reversed, by key, chained...
#[derive(Debug, Clone)]
pub enum Cmp {
    Natural,
    ReverseNatural,
    CaseInsensitive,
    Reverse(ObjId),
    Comparing { key: ObjId, cmp: ObjId, kind: u8 },
    Then(ObjId, ObjId),
    Nulls { first: bool, cmp: ObjId },
    /// Enum constants by ordinal (`EnumMap`, `EnumSet`): the enum's mirror.
    Ordinal(ObjId),
}

pub fn native_comparator(vm: &mut Vm, c: Cmp) -> Res<ObjId> {
    let refs = match &c {
        Cmp::Reverse(x) => vec![Value::Ref(*x)],
        Cmp::Comparing { key, cmp, kind } => vec![Value::Ref(*key), Value::Ref(*cmp), Value::Int(*kind as i32)],
        Cmp::Then(x, y) => vec![Value::Ref(*x), Value::Ref(*y)],
        Cmp::Nulls { first, cmp } => vec![Value::Int(*first as i32), Value::Ref(*cmp)],
        Cmp::Ordinal(mirror) => vec![Value::Ref(*mirror)],
        _ => Vec::new(),
    };
    let tag = match c {
        Cmp::Natural => 0,
        Cmp::ReverseNatural => 1,
        Cmp::CaseInsensitive => 2,
        Cmp::Reverse(_) => 3,
        Cmp::Comparing { .. } => 4,
        Cmp::Then(..) => 5,
        Cmp::Nulls { .. } => 6,
        Cmp::Ordinal(_) => 7,
    };
    let mut items = vec![Value::Int(tag)];
    items.extend(refs);
    new_collection(vm, "java/util/Comparator$Native", State::Generic(items))
}

fn comparator_compare(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Generic(v)) = state(vm, t).cloned() else { return Err(Exc::Internal("not a comparator".into())) };
    let (x, y) = (arg(a, 1), arg(a, 2));
    let r = match v[0].i() {
        0 => compare(vm, x, y, 0)?,
        1 => compare(vm, y, x, 0)?,
        2 => {
            let (p, q) = (super::string::units(vm, x.r())?, super::string::units(vm, y.r())?);
            let ps = String::from_utf16_lossy(&p).to_uppercase().to_lowercase();
            let qs = String::from_utf16_lossy(&q).to_uppercase().to_lowercase();
            let (pu, qu): (Vec<u16>, Vec<u16>) = (ps.encode_utf16().collect(), qs.encode_utf16().collect());
            pu.iter().zip(&qu).find(|(a, b)| a != b).map(|(a, b)| *a as i32 - *b as i32).unwrap_or(pu.len() as i32 - qu.len() as i32)
        }
        3 => compare(vm, y, x, v[1].r())?,
        4 => {
            let (key, cmp, kind) = (v[1].r(), v[2].r(), v[3].i() as u8);
            let (kx, ky) = match kind {
                b'I' => (vm.invoke_virtual(key, "applyAsInt", "(Ljava/lang/Object;)I", &[x])?, vm.invoke_virtual(key, "applyAsInt", "(Ljava/lang/Object;)I", &[y])?),
                b'J' => (vm.invoke_virtual(key, "applyAsLong", "(Ljava/lang/Object;)J", &[x])?, vm.invoke_virtual(key, "applyAsLong", "(Ljava/lang/Object;)J", &[y])?),
                b'D' => (vm.invoke_virtual(key, "applyAsDouble", "(Ljava/lang/Object;)D", &[x])?, vm.invoke_virtual(key, "applyAsDouble", "(Ljava/lang/Object;)D", &[y])?),
                _ => (vm.invoke_virtual(key, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[x])?, vm.invoke_virtual(key, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[y])?),
            };
            match (kx, ky) {
                (Value::Int(p), Value::Int(q)) => (p > q) as i32 - (p < q) as i32,
                (Value::Long(p), Value::Long(q)) => (p > q) as i32 - (p < q) as i32,
                (Value::Double(p), Value::Double(q)) => {
                    let (a, b) = (super::boxes::double_to_long_bits(p), super::boxes::double_to_long_bits(q));
                    if p < q { -1 } else if p > q { 1 } else { (a > b) as i32 - (a < b) as i32 }
                }
                (p, q) => compare(vm, p, q, cmp)?,
            }
        }
        5 => {
            let c = compare(vm, x, y, v[1].r())?;
            if c != 0 { c } else { compare(vm, x, y, v[2].r())? }
        }
        6 => {
            let first = v[1].i() != 0;
            match (x.r() == 0, y.r() == 0) {
                (true, true) => 0,
                (true, false) => if first { -1 } else { 1 },
                (false, true) => if first { 1 } else { -1 },
                _ => if v[2].r() == 0 { 0 } else { compare(vm, x, y, v[2].r())? },
            }
        }
        7 => {
            if x.r() == 0 || y.r() == 0 {
                return Err(vm.npe(None));
            }
            let enum_class = vm.mirror_class(v[1].r()).unwrap_or(0);
            if !vm.instance_of(x.r(), enum_class) || !vm.instance_of(y.r(), enum_class) {
                let bad = if vm.instance_of(x.r(), enum_class) { y.r() } else { x.r() };
                let msg = format!("{} != {}", vm.class_of(bad).map(|c| vm.class(c).java_name()).unwrap_or_default(), vm.class(enum_class).java_name());
                return Err(vm.throw_new("java/lang/ClassCastException", Some(&msg)));
            }
            super::lang::enum_parts(vm, x.r()).1 - super::lang::enum_parts(vm, y.r()).1
        }
        _ => 0,
    };
    Ok(Value::Int(r))
}

fn comparator_default(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let c = match op {
        "reversed" => Cmp::Reverse(t),
        "thenComparing" => {
            let other = arg(a, 1).r();
            let comparator_class = vm.load("java/util/Comparator")?;
            let second = if vm.instance_of(other, comparator_class) { other } else { native_comparator(vm, Cmp::Comparing { key: other, cmp: 0, kind: b'L' })? };
            Cmp::Then(t, second)
        }
        "thenComparingKey" => {
            let second = native_comparator(vm, Cmp::Comparing { key: arg(a, 1).r(), cmp: arg(a, 2).r(), kind: b'L' })?;
            Cmp::Then(t, second)
        }
        "thenComparingInt" | "thenComparingLong" | "thenComparingDouble" => {
            let kind = match op {
                "thenComparingInt" => b'I',
                "thenComparingLong" => b'J',
                _ => b'D',
            };
            let second = native_comparator(vm, Cmp::Comparing { key: arg(a, 1).r(), cmp: 0, kind })?;
            Cmp::Then(t, second)
        }
        _ => return Err(Exc::Unsupported(format!("Comparator.{op}"))),
    };
    ret_ref(native_comparator(vm, c)?)
}

// ------------------------------------------------------------------ functions

/// A function the JDK composes (`andThen`, `negate`, `identity`...), applied
/// through the interface method `call` names.
#[derive(Debug, Clone, Copy)]
enum Fun {
    Identity,
    AndThen,
    Compose,
    Negate,
    And,
    Or,
    IsEqual,
}

fn fun_object(vm: &mut Vm, iface: &str, f: Fun, args: &[Value]) -> Res<Value> {
    let class = format!("{iface}$Native");
    let mut items = vec![Value::Int(f as i32)];
    items.extend_from_slice(args);
    ret_ref(new_collection(vm, &class, State::Generic(items))?)
}

/// Runs a composed function with the argument values of its interface method.
fn fun_call(vm: &mut Vm, a: &[Value], name: &str, desc: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Generic(v)) = state(vm, t).cloned() else { return Err(Exc::Internal("not a function".into())) };
    let args = &a[1..];
    let f = v[0].i();
    let call = |vm: &mut Vm, target: ObjId, args: &[Value]| vm.invoke_virtual(target, name, desc, args);
    Ok(match f {
        0 => args.first().copied().unwrap_or_default(),
        1 => {
            // andThen: the first function, then the second on its result (or
            // after it, for consumers).
            let r = call(vm, v[1].r(), args)?;
            if name == "accept" {
                call(vm, v[2].r(), args)?
            } else {
                let next_desc = desc_one(desc);
                vm.invoke_virtual(v[2].r(), name_one(name), &next_desc, &[r])?
            }
        }
        2 => {
            let r = vm.invoke_virtual(v[2].r(), name, desc, args)?;
            vm.invoke_virtual(v[1].r(), name, desc, &[r])?
        }
        3 => Value::Int((!call(vm, v[1].r(), args)?.z()) as i32),
        4 => Value::Int((call(vm, v[1].r(), args)?.z() && call(vm, v[2].r(), args)?.z()) as i32),
        5 => Value::Int((call(vm, v[1].r(), args)?.z() || call(vm, v[2].r(), args)?.z()) as i32),
        6 => Value::Int(super::lang::objects_equals(vm, v[1].r(), args[0].r())? as i32),
        _ => v[1],
    })
}

/// The method a one-argument function applies: `apply` stays `apply`.
fn name_one(name: &str) -> &str {
    match name {
        "applyAsInt" | "applyAsLong" | "applyAsDouble" => name,
        _ => "apply",
    }
}

fn desc_one(desc: &str) -> String {
    match desc {
        "(II)I" | "(I)I" => "(I)I".into(),
        "(JJ)J" | "(J)J" => "(J)J".into(),
        "(DD)D" | "(D)D" => "(D)D".into(),
        _ => "(Ljava/lang/Object;)Ljava/lang/Object;".into(),
    }
}

macro_rules! functional {
    ($name:literal, $method:literal, $desc:literal, [$($extra:expr),* $(,)?]) => {
        NativeClass {
            name: $name,
            super_name: None,
            interfaces: &[],
            access: INTERFACE,
            methods: &[
                NativeMethod { name: $method, desc: $desc, access: A, f: None },
                $($extra),*
            ],
            statics: &[],
            clinit: None,
            data: None,
        }
    };
}

/// The JDK's own object for a composed function of interface `$name`.
macro_rules! functional_native {
    ($name:literal, $iface:literal, $method:literal, $desc:literal) => {
        NativeClass {
            name: $name,
            super_name: Some("java/lang/Object"),
            interfaces: &[$iface],
            access: FINAL,
            methods: &[NativeMethod { name: $method, desc: $desc, access: M, f: Some(|vm, a| fun_call(vm, a, $method, $desc)) }],
            statics: &[],
            clinit: None,
            data: None,
        }
    };
}

const fn nm(name: &'static str, desc: &'static str, access: u16, f: super::NativeFn) -> NativeMethod {
    NativeMethod { name, desc, access, f: Some(f) }
}

// ------------------------------------------------------------------ Arrays / Collections

fn arrays_to_string(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let r = arg(a, 0).r();
    if r == 0 {
        return super::ret_str(vm, "null");
    }
    let class = vm.class_of(r).unwrap();
    let desc = match vm.class(class).kind {
        crate::vm::ClassKind::Array { elem, .. } if elem != b'L' => (elem as char).to_string(),
        _ => "Ljava/lang/Object;".into(),
    };
    let mut out: Vec<u16> = vec!['[' as u16];
    for (i, v) in vm.array_values(r).into_iter().enumerate() {
        if i > 0 {
            out.extend(", ".encode_utf16());
        }
        out.extend(vm.stringify(v, &desc)?);
    }
    out.push(']' as u16);
    ret_ref(vm.new_string_units(out)?)
}

fn deep_to_string(vm: &mut Vm, r: ObjId, out: &mut Vec<u16>, seen: &mut Vec<ObjId>) -> Res<()> {
    if r == 0 {
        out.extend("null".encode_utf16());
        return Ok(());
    }
    out.push('[' as u16);
    seen.push(r);
    let class = vm.class_of(r).unwrap();
    let elem = match vm.class(class).kind {
        crate::vm::ClassKind::Array { elem, .. } => elem,
        _ => b'L',
    };
    for (i, v) in vm.array_values(r).into_iter().enumerate() {
        if i > 0 {
            out.extend(", ".encode_utf16());
        }
        if elem == b'L' && vm.array(v.r()).is_some() {
            if seen.contains(&v.r()) {
                out.extend("[...]".encode_utf16());
            } else {
                let inner = vm.class_of(v.r()).unwrap();
                if matches!(vm.class(inner).kind, crate::vm::ClassKind::Array { elem: b'L', .. }) {
                    deep_to_string(vm, v.r(), out, seen)?;
                } else {
                    let s = arrays_to_string(vm, &[v])?;
                    out.extend(vm.string_units(s.r()).unwrap_or(&[]).to_vec());
                }
            }
        } else {
            let desc = if elem == b'L' { "Ljava/lang/Object;".to_string() } else { (elem as char).to_string() };
            out.extend(vm.stringify(v, &desc)?);
        }
    }
    seen.pop();
    out.push(']' as u16);
    Ok(())
}

fn primitive_sort(arr: &mut Array, from: usize, to: usize) {
    match arr {
        Array::I(v) => v[from..to].sort_unstable(),
        Array::J(v) => v[from..to].sort_unstable(),
        Array::S(v) => v[from..to].sort_unstable(),
        Array::B(v) => v[from..to].sort_unstable(),
        Array::C(v) => v[from..to].sort_unstable(),
        Array::F(v) => v[from..to].sort_by(|a, b| java_double_order(*a as f64, *b as f64)),
        Array::D(v) => v[from..to].sort_by(|a, b| java_double_order(*a, *b)),
        _ => {}
    }
}

/// `Double.compare`'s total order: -0.0 before 0.0, NaN last.
fn java_double_order(a: f64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Equal,
        (true, false) => Greater,
        (false, true) => Less,
        _ => {
            if a < b {
                Less
            } else if a > b {
                Greater
            } else {
                let (x, y) = (a.to_bits() as i64, b.to_bits() as i64);
                x.cmp(&y)
            }
        }
    }
}

fn arrays_sort(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let r = arg(a, 0).r();
    let Some(arr) = vm.array(r) else { return Err(vm.npe(None)) };
    let len = arr.len();
    let (from, to, cmp) = match &a[1..] {
        [Value::Int(f), Value::Int(t), rest @ ..] => (*f as usize, *t as usize, rest.first().map_or(0, |v| v.r())),
        [Value::Ref(c)] => (0, len, *c),
        _ => (0, len, 0),
    };
    if from > to || to > len {
        let msg = format!("Array index out of range: {to}");
        return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
    }
    if let Some(Array::L(items)) = vm.array(r) {
        let mut vals: Vec<Value> = items[from..to].iter().map(|&x| Value::Ref(x)).collect();
        sort_values(vm, &mut vals, cmp)?;
        for (i, v) in vals.into_iter().enumerate() {
            vm.ref_array_set(r, from + i, v.r());
        }
    } else if let Some(arr) = vm.array_mut(r) {
        primitive_sort(arr, from, to);
    }
    void()
}

fn array_hash(vm: &mut Vm, r: ObjId, deep: bool) -> Res<i32> {
    if r == 0 {
        return Ok(0);
    }
    let class = vm.class_of(r).unwrap();
    let elem = match vm.class(class).kind {
        crate::vm::ClassKind::Array { elem, .. } => elem,
        _ => b'L',
    };
    let mut h: i32 = 1;
    for v in vm.array_values(r) {
        let e = if elem == b'L' {
            if deep && vm.array(v.r()).is_some() { array_hash(vm, v.r(), true)? } else { super::lang::object_hash(vm, v.r())? }
        } else {
            super::lang::hash_of(vm, v, &(elem as char).to_string())?
        };
        h = h.wrapping_mul(31).wrapping_add(e);
    }
    Ok(h)
}

/// The element kind of an array object (`L` for references).
fn elem_kind(vm: &Vm, r: ObjId) -> u8 {
    match vm.class_of(r).map(|c| &vm.class(c).kind) {
        Some(crate::vm::ClassKind::Array { elem, .. }) => *elem,
        _ => b'L',
    }
}

/// `Arrays.compare` and `compareUnsigned`: the first differing elements
/// compared as their wrapper's `compare` does, else the length difference.
fn arrays_compare(vm: &mut Vm, a: &[Value], unsigned: bool) -> Res<Value> {
    let (x, y, cmp) = (arg(a, 0).r(), arg(a, 1).r(), arg(a, 2).r());
    if x == y {
        return Ok(Value::Int(0));
    }
    if x == 0 || y == 0 {
        return Ok(Value::Int(if x == 0 { -1 } else { 1 }));
    }
    let kind = elem_kind(vm, x);
    let (p, q) = (vm.array_values(x), vm.array_values(y));
    for (u, v) in p.iter().zip(&q) {
        let c = match kind {
            b'Z' => (u.i() != 0) as i32 - (v.i() != 0) as i32,
            b'B' if unsigned => (u.i() as u8 as i32) - (v.i() as u8 as i32),
            b'S' if unsigned => (u.i() as u16 as i32) - (v.i() as u16 as i32),
            b'B' | b'S' | b'C' => u.i() - v.i(),
            b'I' if unsigned => (u.i() as u32).cmp(&(v.i() as u32)) as i32,
            b'I' => u.i().cmp(&v.i()) as i32,
            b'J' if unsigned => (u.j() as u64).cmp(&(v.j() as u64)) as i32,
            b'J' => u.j().cmp(&v.j()) as i32,
            b'F' => java_double_order(u.f() as f64, v.f() as f64) as i32,
            b'D' => java_double_order(u.d(), v.d()) as i32,
            _ => {
                if u.r() == v.r() {
                    0
                } else if u.r() == 0 || v.r() == 0 {
                    if u.r() == 0 { -1 } else { 1 }
                } else {
                    compare(vm, *u, *v, cmp)?
                }
            }
        };
        if c != 0 {
            return Ok(Value::Int(c));
        }
    }
    Ok(Value::Int(p.len() as i32 - q.len() as i32))
}

fn arrays_mismatch(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let (x, y) = (arg(a, 0).r(), arg(a, 1).r());
    if x == 0 || y == 0 {
        return Err(vm.npe(None));
    }
    let kind = elem_kind(vm, x);
    let (p, q) = (vm.array_values(x), vm.array_values(y));
    for (i, (u, v)) in p.iter().zip(&q).enumerate() {
        let same = match kind {
            b'F' => super::boxes::float_to_int_bits(u.f()) == super::boxes::float_to_int_bits(v.f()),
            b'D' => super::boxes::double_to_long_bits(u.d()) == super::boxes::double_to_long_bits(v.d()),
            b'L' => equals(vm, *u, *v)?,
            _ => u == v,
        };
        if !same {
            return Ok(Value::Int(i as i32));
        }
    }
    Ok(Value::Int(if p.len() == q.len() { -1 } else { p.len().min(q.len()) as i32 }))
}

fn arrays_equals(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let (x, y) = (arg(a, 0).r(), arg(a, 1).r());
    if x == y {
        return ret_bool(true);
    }
    if x == 0 || y == 0 {
        return ret_bool(false);
    }
    let (p, q) = (vm.array_values(x), vm.array_values(y));
    if p.len() != q.len() {
        return ret_bool(false);
    }
    let is_ref = matches!(vm.array(x), Some(Array::L(_)));
    for (u, v) in p.into_iter().zip(q) {
        let same = match (u, v) {
            (Value::Float(f), Value::Float(g)) => super::boxes::float_to_int_bits(f) == super::boxes::float_to_int_bits(g),
            (Value::Double(f), Value::Double(g)) => super::boxes::double_to_long_bits(f) == super::boxes::double_to_long_bits(g),
            _ if is_ref => equals(vm, u, v)?,
            _ => u == v,
        };
        if !same {
            return ret_bool(false);
        }
    }
    ret_bool(true)
}

fn deep_equals(vm: &mut Vm, x: ObjId, y: ObjId) -> Res<bool> {
    if x == y {
        return Ok(true);
    }
    if x == 0 || y == 0 {
        return Ok(false);
    }
    if vm.array(x).is_some() && vm.array(y).is_some() {
        if !matches!(vm.array(x), Some(Array::L(_))) {
            return Ok(arrays_equals(vm, &[Value::Ref(x), Value::Ref(y)])?.z());
        }
        let (p, q) = (vm.ref_elements(x), vm.ref_elements(y));
        if p.len() != q.len() {
            return Ok(false);
        }
        for (u, v) in p.into_iter().zip(q) {
            if !deep_equals(vm, u, v)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    super::lang::objects_equals(vm, x, y)
}

fn copy_of(vm: &mut Vm, a: &[Value], range: bool) -> Res<Value> {
    let r = arg(a, 0).r();
    let Some(class) = vm.class_of(r) else { return Err(vm.npe(None)) };
    let values = vm.array_values(r);
    let (from, to) = if range { (arg(a, 1).i(), arg(a, 2).i()) } else { (0, arg(a, 1).i()) };
    if from < 0 || from as usize > values.len() {
        let msg = format!("Array index out of range: {from}");
        return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
    }
    if to < from {
        if range {
            let msg = format!("{from} > {to}");
            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
        }
        let msg = format!("{to}");
        return Err(vm.throw_new("java/lang/NegativeArraySizeException", Some(&msg)));
    }
    let new_class = match a.get(if range { 3 } else { 2 }) {
        Some(Value::Ref(c)) if *c != 0 => vm.mirror_class(*c).unwrap_or(class),
        _ => class,
    };
    let n = to - from;
    let out = vm.new_array(new_class, n)?;
    for i in 0..n as usize {
        if let Some(v) = values.get(from as usize + i) {
            vm.array_mut(out).unwrap().set(i, *v);
        }
    }
    ret_ref(out)
}

fn arrays_fill(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let r = arg(a, 0).r();
    let Some(len) = vm.array(r).map(Array::len) else { return Err(vm.npe(None)) };
    let (from, to, v) = if a.len() >= 4 { (arg(a, 1).i() as usize, arg(a, 2).i() as usize, arg(a, 3)) } else { (0, len, arg(a, 1)) };
    if from > to || to > len {
        let msg = format!("Array index out of range: {to}");
        return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
    }
    if let (Some(Array::L(_)), Value::Ref(x)) = (vm.array(r), v)
        && x != 0
    {
        let comp = match vm.class(vm.class_of(r).unwrap()).kind {
            crate::vm::ClassKind::Array { component, .. } => component,
            _ => 0,
        };
        if !vm.instance_of(x, comp) {
            let name = vm.class(vm.class_of(x).unwrap()).java_name();
            return Err(vm.throw_new("java/lang/ArrayStoreException", Some(&name)));
        }
    }
    let arr = vm.array_mut(r).unwrap();
    for i in from..to {
        arr.set(i, v);
    }
    void()
}

fn binary_search(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let r = arg(a, 0).r();
    let values = vm.array_values(r);
    let (from, to, key, cmp) = if a.len() >= 4 && matches!(a[1], Value::Int(_)) && matches!(a[2], Value::Int(_)) {
        (arg(a, 1).i() as usize, arg(a, 2).i() as usize, arg(a, 3), arg(a, 4).r())
    } else {
        (0, values.len(), arg(a, 1), arg(a, 2).r())
    };
    let (mut lo, mut hi) = (from as i64, to as i64 - 1);
    while lo <= hi {
        let mid = ((lo + hi) as u64 >> 1) as i64;
        let v = values[mid as usize];
        let c = match (v, key) {
            (Value::Int(x), Value::Int(y)) => (x > y) as i32 - (x < y) as i32,
            (Value::Long(x), Value::Long(y)) => (x > y) as i32 - (x < y) as i32,
            (Value::Double(x), Value::Double(y)) => java_double_order(x, y) as i32,
            (Value::Float(x), Value::Float(y)) => java_double_order(x as f64, y as f64) as i32,
            _ => compare(vm, v, key, cmp)?,
        };
        if c < 0 {
            lo = mid + 1;
        } else if c > 0 {
            hi = mid - 1;
        } else {
            return Ok(Value::Int(mid as i32));
        }
    }
    Ok(Value::Int(-(lo as i32 + 1)))
}

fn as_list(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let arr = arg(a, 0).r();
    if arr == 0 {
        return Err(vm.npe(None));
    }
    let items = vm.array_values(arr);
    ret_ref(new_collection(vm, "java/util/Arrays$ArrayList", State::List(JList { items, fixed: true, backing: arr, ..Default::default() }))?)
}

pub fn immutable_list(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    if items.iter().any(|v| v.r() == 0) {
        return Err(vm.npe(None));
    }
    ret_ref(new_immutable_list(vm, items)?)
}

pub fn immutable_set(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    if items.iter().any(|v| v.r() == 0) {
        return Err(vm.npe(None));
    }
    // Set.of's order depends on a per-run salt in HotSpot: any fixed order is
    // as correct, and this one is the VM's always.
    let r = new_collection(vm, "java/util/ImmutableCollections$SetN", State::Map(Box::new({
        let mut m = JMap::new();
        m.linked = true;
        m.set = true;
        m
    })))?;
    for v in items {
        if map_put(vm, r, v, Value::Int(1))?.is_some() {
            let s = String::from_utf16_lossy(&vm.stringify(v, "Ljava/lang/Object;")?);
            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&format!("duplicate element: {s}"))));
        }
    }
    if let Some(State::Map(m)) = state_mut(vm, r) {
        m.immutable = true;
    }
    ret_ref(r)
}

pub fn immutable_map(vm: &mut Vm, pairs: Vec<(Value, Value)>) -> Res<Value> {
    let r = new_collection(vm, "java/util/ImmutableCollections$MapN", State::Map(Box::new({
        let mut m = JMap::new();
        m.linked = true;
        m
    })))?;
    for (k, v) in pairs {
        if k.r() == 0 || v.r() == 0 {
            return Err(vm.npe(None));
        }
        if map_put(vm, r, k, v)?.is_some() {
            let s = String::from_utf16_lossy(&vm.stringify(k, "Ljava/lang/Object;")?);
            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&format!("duplicate key: {s}"))));
        }
    }
    if let Some(State::Map(m)) = state_mut(vm, r) {
        m.immutable = true;
    }
    ret_ref(r)
}

fn unmodifiable(vm: &mut Vm, source: ObjId, kind: &str) -> Res<Value> {
    if source == 0 {
        return Err(vm.npe(None));
    }
    match kind {
        "list" => {
            let items = iterate(vm, source)?;
            ret_ref(new_immutable_list(vm, items)?)
        }
        "set" => {
            let items = iterate(vm, source)?;
            let r = new_collection(vm, "java/util/Collections$UnmodifiableSet", State::Map(Box::new({
                let mut m = JMap::new();
                m.linked = true;
                m.set = true;
                m
            })))?;
            for v in items {
                map_put(vm, r, v, Value::Int(1))?;
            }
            if let Some(State::Map(m)) = state_mut(vm, r) {
                m.immutable = true;
            }
            ret_ref(r)
        }
        _ => {
            let pairs = map_pairs(vm, source)?;
            let r = new_collection(vm, "java/util/Collections$UnmodifiableMap", State::Map(Box::new({
                let mut m = JMap::new();
                m.linked = true;
                m
            })))?;
            for (k, v) in pairs {
                map_put(vm, r, k, v)?;
            }
            if let Some(State::Map(m)) = state_mut(vm, r) {
                m.immutable = true;
            }
            ret_ref(r)
        }
    }
}

fn collections_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "sort" => {
            let l = arg(a, 0).r();
            vm.invoke_virtual(l, "sort", "(Ljava/util/Comparator;)V", &[Value::Ref(arg(a, 1).r())])?;
            void()
        }
        "reverse" | "shuffle" | "swap" | "rotate" => {
            let l = arg(a, 0).r();
            let mut items = iterate(vm, l)?;
            match op {
                "reverse" => items.reverse(),
                "swap" => {
                    let (i, j) = (arg(a, 1).i(), arg(a, 2).i());
                    if i < 0 || j < 0 || i as usize >= items.len() || j as usize >= items.len() {
                        return Err(ioobe(vm, i.max(j), items.len()));
                    }
                    items.swap(i as usize, j as usize);
                }
                "rotate" => {
                    let n = items.len() as i64;
                    if n > 0 {
                        let d = (arg(a, 1).i() as i64).rem_euclid(n) as usize;
                        items.rotate_right(d);
                    }
                }
                _ => {
                    let rnd = match a.get(1) {
                        Some(Value::Ref(r)) if *r != 0 => *r,
                        _ => shared_random(vm)?,
                    };
                    for i in (1..items.len()).rev() {
                        let j = random_next_int_bound(vm, rnd, i as i32 + 1)?;
                        items.swap(i, j as usize);
                    }
                }
            }
            for (i, v) in items.into_iter().enumerate() {
                vm.invoke_virtual(l, "set", "(ILjava/lang/Object;)Ljava/lang/Object;", &[Value::Int(i as i32), v])?;
            }
            void()
        }
        "max" | "min" => {
            let items = iterate(vm, arg(a, 0).r())?;
            let cmp = arg(a, 1).r();
            let Some(&first) = items.first() else { return Err(vm.throw_new("java/util/NoSuchElementException", None)) };
            let mut best = first;
            for v in &items[1..] {
                let c = compare(vm, *v, best, cmp)?;
                if (op == "max" && c > 0) || (op == "min" && c < 0) {
                    best = *v;
                }
            }
            Ok(best)
        }
        "frequency" => {
            let mut n = 0;
            for v in iterate(vm, arg(a, 0).r())? {
                if equals(vm, arg(a, 1), v)? {
                    n += 1;
                }
            }
            Ok(Value::Int(n))
        }
        "nCopies" => {
            let n = arg(a, 0).i();
            if n < 0 {
                let msg = format!("List length = {n}");
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
            }
            ret_ref(new_collection(vm, "java/util/Collections$CopiesList", State::List(JList { items: vec![arg(a, 1); n as usize], immutable: true, ..Default::default() }))?)
        }
        "addAll" => {
            let c = arg(a, 0).r();
            let mut changed = false;
            for v in vm.array_values(arg(a, 1).r()) {
                changed |= vm.invoke_virtual(c, "add", "(Ljava/lang/Object;)Z", &[v])?.z();
            }
            ret_bool(changed)
        }
        "disjoint" => {
            let x = iterate(vm, arg(a, 0).r())?;
            for v in x {
                if vm.invoke_virtual(arg(a, 1).r(), "contains", "(Ljava/lang/Object;)Z", &[v])?.z() {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        "binarySearch" => {
            let items = iterate(vm, arg(a, 0).r())?;
            let refs: Vec<ObjId> = items.iter().map(|v| v.r()).collect();
            let arr = vm.new_ref_array("Ljava/lang/Object;", &refs)?;
            binary_search(vm, &[Value::Ref(arr), arg(a, 1), arg(a, 2)])
        }
        _ => Err(Exc::Unsupported(format!("Collections.{op}"))),
    }
}

// ------------------------------------------------------------------ Objects / Optional

pub fn optional_of(vm: &mut Vm, v: Value) -> Res<Value> {
    ret_ref(new_collection(vm, "java/util/Optional", State::Optional(v, v.r() != 0))?)
}

fn optional_parts(vm: &Vm, r: ObjId) -> (Value, bool) {
    match state(vm, r) {
        Some(State::Optional(v, p)) => (*v, *p),
        _ => (Value::NULL, false),
    }
}

fn optional_op(vm: &mut Vm, a: &[Value], op: &str, kind: u8) -> Res<Value> {
    let t = this(vm, a)?;
    let (v, present) = optional_parts(vm, t);
    let class = match kind {
        b'I' => "java/util/OptionalInt",
        b'J' => "java/util/OptionalLong",
        b'D' => "java/util/OptionalDouble",
        _ => "java/util/Optional",
    };
    match op {
        "isPresent" => ret_bool(present),
        "isEmpty" => ret_bool(!present),
        "get" | "orElseThrow" | "getAsInt" | "getAsLong" | "getAsDouble" => {
            if present {
                Ok(v)
            } else {
                Err(vm.throw_new("java/util/NoSuchElementException", Some("No value present")))
            }
        }
        "orElseThrowSupplier" => {
            if present {
                Ok(v)
            } else {
                let ex = vm.invoke_virtual(arg(a, 1).r(), "get", "()Ljava/lang/Object;", &[])?.r();
                Err(Exc::Throw(ex))
            }
        }
        "orElse" => Ok(if present { v } else { arg(a, 1) }),
        "orElseGet" => {
            if present {
                Ok(v)
            } else {
                let (name, desc) = match kind {
                    b'I' => ("getAsInt", "()I"),
                    b'J' => ("getAsLong", "()J"),
                    b'D' => ("getAsDouble", "()D"),
                    _ => ("get", "()Ljava/lang/Object;"),
                };
                vm.invoke_virtual(arg(a, 1).r(), name, desc, &[])
            }
        }
        "ifPresent" => {
            if present {
                let desc = match kind {
                    b'I' => "(I)V",
                    b'J' => "(J)V",
                    b'D' => "(D)V",
                    _ => "(Ljava/lang/Object;)V",
                };
                vm.invoke_virtual(arg(a, 1).r(), "accept", desc, &[v])?;
            }
            void()
        }
        "ifPresentOrElse" => {
            if present {
                let desc = match kind {
                    b'I' => "(I)V",
                    b'J' => "(J)V",
                    b'D' => "(D)V",
                    _ => "(Ljava/lang/Object;)V",
                };
                vm.invoke_virtual(arg(a, 1).r(), "accept", desc, &[v])?;
            } else {
                vm.invoke_virtual(arg(a, 2).r(), "run", "()V", &[])?;
            }
            void()
        }
        "map" => {
            if !present {
                return optional_of(vm, Value::NULL);
            }
            let r = vm.invoke_virtual(arg(a, 1).r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?;
            optional_of(vm, r)
        }
        "flatMap" => {
            if !present {
                return optional_of(vm, Value::NULL);
            }
            let r = vm.invoke_virtual(arg(a, 1).r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?;
            if r.r() == 0 {
                return Err(vm.npe(None));
            }
            Ok(r)
        }
        "filter" => {
            if present && vm.invoke_virtual(arg(a, 1).r(), "test", "(Ljava/lang/Object;)Z", &[v])?.z() {
                ret_ref(t)
            } else {
                optional_of(vm, Value::NULL)
            }
        }
        "or" => {
            if present {
                ret_ref(t)
            } else {
                vm.invoke_virtual(arg(a, 1).r(), "get", "()Ljava/lang/Object;", &[])
            }
        }
        "stream" => {
            let items = if present { vec![v] } else { Vec::new() };
            super::stream::stream_of(vm, items)
        }
        "equals" => {
            let o = arg(a, 1).r();
            if o == t {
                return ret_bool(true);
            }
            if o == 0 || vm.class_of(o) != vm.class_of(t) {
                return ret_bool(false);
            }
            let (w, p) = optional_parts(vm, o);
            ret_bool(present == p && (!present || (if kind == b'L' { equals(vm, v, w)? } else { v == w })))
        }
        "hashCode" => {
            if !present {
                return Ok(Value::Int(0));
            }
            let desc = match kind {
                b'I' => "I",
                b'J' => "J",
                b'D' => "D",
                _ => "Ljava/lang/Object;",
            };
            Ok(Value::Int(super::lang::hash_of(vm, v, desc)?))
        }
        "toString" => {
            let name = class.rsplit('/').next().unwrap();
            if !present {
                return ret_str(vm, &format!("{name}.empty"));
            }
            let desc = match kind {
                b'I' => "I",
                b'J' => "J",
                b'D' => "D",
                _ => "Ljava/lang/Object;",
            };
            let inner = String::from_utf16_lossy(&vm.stringify(v, desc)?);
            ret_str(vm, &format!("{name}[{inner}]"))
        }
        _ => Err(Exc::Unsupported(format!("Optional.{op}"))),
    }
}

pub fn new_optional(vm: &mut Vm, class: &str, v: Value, present: bool) -> Res<Value> {
    ret_ref(new_collection(vm, class, State::Optional(v, present))?)
}

fn objects_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "equals" => ret_bool(equals(vm, arg(a, 0), arg(a, 1))?),
        "deepEquals" => ret_bool(deep_equals(vm, arg(a, 0).r(), arg(a, 1).r())?),
        "hashCode" => Ok(Value::Int(super::lang::object_hash(vm, arg(a, 0).r())?)),
        "hash" => Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?)),
        "toString" => {
            if arg(a, 0).r() == 0 && a.len() > 1 {
                return Ok(arg(a, 1));
            }
            let u = vm.stringify(arg(a, 0), "Ljava/lang/Object;")?;
            ret_ref(vm.new_string_units(u)?)
        }
        "isNull" => ret_bool(arg(a, 0).r() == 0),
        "nonNull" => ret_bool(arg(a, 0).r() != 0),
        "requireNonNull" => {
            if arg(a, 0).r() != 0 {
                return Ok(arg(a, 0));
            }
            let msg = match a.get(1) {
                Some(Value::Ref(m)) if *m != 0 => {
                    let string = vm.known.string;
                    if vm.instance_of(*m, string) {
                        vm.string(*m)
                    } else {
                        let s = vm.invoke_virtual(*m, "get", "()Ljava/lang/Object;", &[])?;
                        vm.string(s.r())
                    }
                }
                _ => None,
            };
            Err(vm.npe(msg.as_deref()))
        }
        "requireNonNullElse" => {
            if arg(a, 0).r() != 0 {
                Ok(arg(a, 0))
            } else if arg(a, 1).r() != 0 {
                Ok(arg(a, 1))
            } else {
                Err(vm.npe(Some("defaultObj")))
            }
        }
        "requireNonNullElseGet" => {
            if arg(a, 0).r() != 0 {
                return Ok(arg(a, 0));
            }
            let v = vm.invoke_virtual(arg(a, 1).r(), "get", "()Ljava/lang/Object;", &[])?;
            if v.r() == 0 {
                return Err(vm.npe(Some("supplier.get()")));
            }
            Ok(v)
        }
        "compare" => {
            if arg(a, 0).r() == arg(a, 1).r() {
                return Ok(Value::Int(0));
            }
            Ok(Value::Int(compare(vm, arg(a, 0), arg(a, 1), arg(a, 2).r())?))
        }
        "checkIndex" => {
            let (i, n) = (arg(a, 0).i(), arg(a, 1).i());
            if i < 0 || i >= n {
                let msg = format!("Index {i} out of bounds for length {n}");
                return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
            }
            Ok(Value::Int(i))
        }
        _ => Err(Exc::Unsupported(format!("Objects.{op}"))),
    }
}

// ------------------------------------------------------------------ Random

const MULTIPLIER: i64 = 0x5DEECE66D;
const MASK: i64 = (1 << 48) - 1;

fn scramble(seed: i64) -> u64 {
    ((seed ^ MULTIPLIER) & MASK) as u64
}

fn random_next(vm: &mut Vm, r: ObjId, bits: u32) -> Res<i32> {
    // A bytecode subclass may override next(int).
    if let Some(c) = vm.class_of(r)
        && !matches!(vm.class(c).kind, crate::vm::ClassKind::Native(_))
    {
        let nd = vm.nd("next", "(I)I");
        let m = vm.select(c, nd)?;
        if !matches!(vm.method(m).body, crate::vm::MethodBody::Native(_)) {
            return Ok(vm.invoke(m, &[Value::Ref(r), Value::Int(bits as i32)])?.i());
        }
    }
    let Some(State::Random(seed, _)) = state_mut(vm, r) else { return Err(Exc::Internal("not a Random".into())) };
    let next = (seed.wrapping_mul(MULTIPLIER as u64).wrapping_add(0xB)) & MASK as u64;
    *seed = next;
    Ok((next >> (48 - bits)) as i64 as i32)
}

pub fn random_next_int_bound(vm: &mut Vm, r: ObjId, bound: i32) -> Res<i32> {
    if bound <= 0 {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("bound must be positive")));
    }
    let mut v = random_next(vm, r, 31)?;
    let m = bound - 1;
    if bound & m == 0 {
        return Ok(((bound as i64 * v as i64) >> 31) as i32);
    }
    let mut u = v;
    loop {
        v = u % bound;
        if u.wrapping_sub(v).wrapping_add(m) >= 0 {
            return Ok(v);
        }
        u = random_next(vm, r, 31)?;
    }
}

fn random_next_long(vm: &mut Vm, r: ObjId) -> Res<i64> {
    let hi = random_next(vm, r, 32)? as i64;
    let lo = random_next(vm, r, 32)? as i64;
    Ok((hi << 32).wrapping_add(lo))
}

pub fn random_next_double(vm: &mut Vm, r: ObjId) -> Res<Value> {
    let hi = random_next(vm, r, 26)? as i64;
    let lo = random_next(vm, r, 27)? as i64;
    Ok(Value::Double(((hi << 27) + lo) as f64 * (1.0 / (1u64 << 53) as f64)))
}

/// The `Random` `Math.random()` uses.
pub fn shared_random(vm: &mut Vm) -> Res<ObjId> {
    if vm.jdk.threads.math_random == 0 {
        let seed = seed_uniquifier(vm) ^ (1_000_000_000 + vm.jdk.nanos as i64);
        let r = new_collection(vm, "java/util/Random", State::Random(scramble(seed), None))?;
        vm.pinned.push(r);
        vm.jdk.threads.math_random = r;
    }
    Ok(vm.jdk.threads.math_random)
}

fn seed_uniquifier(vm: &mut Vm) -> i64 {
    if vm.jdk.seed_uniquifier == 0 {
        vm.jdk.seed_uniquifier = 8682522807148012;
    }
    vm.jdk.seed_uniquifier = vm.jdk.seed_uniquifier.wrapping_mul(1181783497276652981);
    vm.jdk.seed_uniquifier as i64
}

fn random_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "init" => {
            let seed = match a.get(1) {
                Some(Value::Long(s)) => *s,
                _ => seed_uniquifier(vm) ^ (1_000_000_000 + vm.jdk.nanos as i64 + vm.instructions as i64),
            };
            if let Some(o) = vm.obj_mut(t) {
                o.data = Data::Jdk(Box::new(State::Random(scramble(seed), None)));
            }
            void()
        }
        "setSeed" => {
            if let Some(State::Random(s, g)) = state_mut(vm, t) {
                *s = scramble(arg(a, 1).j());
                *g = None;
            }
            void()
        }
        "next" => Ok(Value::Int(random_next(vm, t, arg(a, 1).i() as u32)?)),
        "nextInt" => Ok(Value::Int(random_next(vm, t, 32)?)),
        "nextIntBound" => Ok(Value::Int(random_next_int_bound(vm, t, arg(a, 1).i())?)),
        "nextIntRange" => {
            let (origin, bound) = (arg(a, 1).i(), arg(a, 2).i());
            if origin >= bound {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("bound must be greater than origin")));
            }
            let mut r = random_next(vm, t, 32)?;
            let n = bound.wrapping_sub(origin);
            let m = n.wrapping_sub(1);
            if n & m == 0 {
                r = (r & m).wrapping_add(origin);
            } else if n > 0 {
                let mut u = ((r as u32) >> 1) as i32;
                loop {
                    r = u % n;
                    if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                        break;
                    }
                    u = ((random_next(vm, t, 32)? as u32) >> 1) as i32;
                }
                r = r.wrapping_add(origin);
            } else {
                while r < origin || r >= bound {
                    r = random_next(vm, t, 32)?;
                }
            }
            Ok(Value::Int(r))
        }
        "nextLong" => Ok(Value::Long(random_next_long(vm, t)?)),
        "nextLongBound" => {
            let bound = arg(a, 1).j();
            if bound <= 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("bound must be positive")));
            }
            let m = bound - 1;
            let mut r = random_next_long(vm, t)?;
            if bound & m == 0 {
                r &= m;
            } else {
                let mut u = ((r as u64) >> 1) as i64;
                loop {
                    r = u % bound;
                    if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                        break;
                    }
                    u = ((random_next_long(vm, t)? as u64) >> 1) as i64;
                }
            }
            Ok(Value::Long(r))
        }
        "nextDouble" => random_next_double(vm, t),
        "nextDoubleRange" => {
            let (origin, bound) = (arg(a, 1).d(), arg(a, 2).d());
            let mut r = random_next_double(vm, t)?.d();
            r = r * (bound - origin) + origin;
            if r >= bound {
                r = f64::from_bits(bound.to_bits() - 1);
            }
            Ok(Value::Double(r))
        }
        "nextFloat" => Ok(Value::Float(random_next(vm, t, 24)? as f32 / (1 << 24) as f32)),
        "nextBoolean" => ret_bool(random_next(vm, t, 1)? != 0),
        "nextGaussian" => {
            if let Some(State::Random(_, g)) = state_mut(vm, t)
                && let Some(v) = g.take()
            {
                return Ok(Value::Double(v));
            }
            loop {
                let v1 = 2.0 * random_next_double(vm, t)?.d() - 1.0;
                let v2 = 2.0 * random_next_double(vm, t)?.d() - 1.0;
                let s = v1 * v1 + v2 * v2;
                if s < 1.0 && s != 0.0 {
                    let multiplier = (-2.0 * libm::log(s) / s).sqrt();
                    if let Some(State::Random(_, g)) = state_mut(vm, t) {
                        *g = Some(v2 * multiplier);
                    }
                    return Ok(Value::Double(v1 * multiplier));
                }
            }
        }
        "nextBytes" => {
            let arr = arg(a, 1).r();
            let len = vm.array(arr).map_or(0, Array::len);
            let mut i = 0;
            while i < len {
                let mut rnd = random_next(vm, t, 32)?;
                let n = (len - i).min(4);
                for _ in 0..n {
                    vm.array_mut(arr).unwrap().set(i, Value::Int(rnd as i8 as i32));
                    rnd >>= 8;
                    i += 1;
                }
            }
            void()
        }
        "ints" | "doubles" => {
            // ints(n), ints(n, origin, bound), doubles(n): finite streams.
            let n = match a.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 100_000,
            };
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(if op == "ints" {
                    if a.len() >= 4 {
                        random_op(vm, &[Value::Ref(t), arg(a, 2), arg(a, 3)], "nextIntRange")?
                    } else {
                        Value::Int(random_next(vm, t, 32)?)
                    }
                } else {
                    random_next_double(vm, t)?
                });
            }
            if op == "ints" { super::stream::int_stream_of(vm, items) } else { super::stream::double_stream_of(vm, items) }
        }
        _ => Err(Exc::Unsupported(format!("Random.{op}"))),
    }
}

// ------------------------------------------------------------------ regex

/// Java's pattern syntax in the `regex` crate's: the character classes
/// Java keeps ASCII unless asked, and possessive quantifiers made greedy.
fn translate_pattern(p: &str, flags: i32) -> String {
    let mut out = String::new();
    if flags & 2 != 0 {
        out.push_str("(?i)");
    }
    if flags & 8 != 0 {
        out.push_str("(?m)");
    }
    if flags & 32 != 0 {
        out.push_str("(?s)");
    }
    if flags & 4 != 0 {
        out.push_str("(?x)");
    }
    let unicode = flags & 256 != 0;
    let mut chars = p.chars().peekable();
    let mut in_class = 0;
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('d') if !unicode => out.push_str(if in_class > 0 { "0-9" } else { "[0-9]" }),
                Some('D') if !unicode => out.push_str("[^0-9]"),
                Some('w') if !unicode => out.push_str(if in_class > 0 { "a-zA-Z_0-9" } else { "[a-zA-Z_0-9]" }),
                Some('W') if !unicode => out.push_str("[^a-zA-Z_0-9]"),
                Some('s') if !unicode => out.push_str(if in_class > 0 { " \\t\\n\\x0B\\f\\r" } else { "[ \\t\\n\\x0B\\f\\r]" }),
                Some('S') if !unicode => out.push_str("[^ \\t\\n\\x0B\\f\\r]"),
                Some('Q') => {
                    let mut lit = String::new();
                    while let Some(x) = chars.next() {
                        if x == '\\' && chars.peek() == Some(&'E') {
                            chars.next();
                            break;
                        }
                        lit.push(x);
                    }
                    out.push_str(&regex::escape(&lit));
                }
                Some('p') if chars.peek() == Some(&'{') => {
                    let mut name = String::new();
                    chars.next();
                    for x in chars.by_ref() {
                        if x == '}' {
                            break;
                        }
                        name.push(x);
                    }
                    let class = match name.as_str() {
                        "Alpha" | "IsAlphabetic" => "a-zA-Z".to_string(),
                        "Digit" | "IsDigit" => "0-9".to_string(),
                        "Alnum" => "a-zA-Z0-9".to_string(),
                        "Upper" | "Lu" | "IsUppercase" => "A-Z".to_string(),
                        "Lower" | "Ll" | "IsLowercase" => "a-z".to_string(),
                        "Punct" => "!-/:-@\\[-`{-~".to_string(),
                        "Space" | "IsWhite_Space" => " \\t\\n\\x0B\\f\\r".to_string(),
                        "XDigit" => "0-9a-fA-F".to_string(),
                        "ASCII" => "\\x00-\\x7F".to_string(),
                        "L" | "IsL" | "IsLetter" => "\\p{L}".to_string(),
                        other => format!("\\p{{{other}}}"),
                    };
                    if in_class > 0 { out.push_str(&class) } else { out.push_str(&format!("[{class}]")) }
                }
                Some('h') => out.push_str("[ \\t\\xA0\\u{1680}\\u{180e}\\u{2000}-\\u{200a}\\u{202f}\\u{205f}\\u{3000}]"),
                Some('R') => out.push_str("(?:\\r\\n|[\\n\\x0B\\f\\r\\u{85}\\u{2028}\\u{2029}])"),
                Some('Z') => out.push_str("(?:\\n?\\z)"),
                Some(x) => {
                    out.push('\\');
                    out.push(x);
                }
                None => out.push('\\'),
            },
            '[' => {
                in_class += 1;
                out.push('[');
                if chars.peek() == Some(&'^') {
                    out.push(chars.next().unwrap());
                }
                if chars.peek() == Some(&']') {
                    chars.next();
                    out.push_str("\\]");
                }
            }
            ']' if in_class > 0 => {
                in_class -= 1;
                out.push(']');
            }
            '~' if in_class > 0 => out.push_str("\\~"),
            '+' if in_class == 0 && matches!(out.chars().last(), Some('*' | '+' | '?' | '}')) && !out.ends_with("\\+") => {
                // A possessive quantifier: kept greedy.
            }
            _ => out.push(c),
        }
    }
    out
}

pub fn compile_regex(vm: &mut Vm, pattern: &str, flags: i32) -> Res<Regex> {
    let translated = translate_pattern(pattern, flags);
    if flags & 16 != 0 {
        let literal = regex::escape(pattern);
        let compiled = regex::Regex::new(&literal).map_err(|e| vm.throw_new("java/util/regex/PatternSyntaxException", Some(&e.to_string())))?;
        return Ok(Regex { source: pattern.into(), flags, names: vec![None], compiled, input: Vec::new(), text: String::new(), groups: Vec::new(), from: 0, append_at: 0, pattern: 0 });
    }
    match regex::Regex::new(&translated) {
        Ok(compiled) => {
            let names = compiled.capture_names().map(|n| n.map(str::to_string)).collect();
            Ok(Regex { source: pattern.into(), flags, names, compiled, input: Vec::new(), text: String::new(), groups: Vec::new(), from: 0, append_at: 0, pattern: 0 })
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("look-around") || msg.contains("backreferences") {
                return Err(Exc::Unsupported(format!("regex {pattern}: {msg}")));
            }
            Err(vm.throw_new("java/util/regex/PatternSyntaxException", Some(&format!("{msg} near index 0\n{pattern}"))))
        }
    }
}

/// Byte offsets of a UTF-8 text mapped back to UTF-16 indices.
fn utf16_index(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

pub fn split(vm: &mut Vm, s: ObjId, pattern: ObjId, limit: i32) -> Res<Value> {
    let units = super::string::units(vm, s)?;
    let p = vm.string_nn(pattern)?;
    let text = String::from_utf16_lossy(&units);
    // String.split's fast path for a one-character literal.
    let pieces: Vec<String> = {
        let re = compile_regex(vm, &p, 0)?;
        let mut out = Vec::new();
        let mut last = 0;
        for m in re.compiled.find_iter(&text) {
            if limit > 0 && out.len() as i32 == limit - 1 {
                break;
            }
            if m.end() == 0 && m.start() == 0 {
                // A zero-width match at the start makes no leading empty piece.
                continue;
            }
            if m.start() == m.end() && m.start() >= text.len() {
                break;
            }
            out.push(text[last..m.start()].to_string());
            last = m.end();
        }
        out.push(text[last..].to_string());
        if limit == 0 {
            while out.len() > 1 && out.last().is_some_and(String::is_empty) {
                out.pop();
            }
            if out.len() == 1 && out[0].is_empty() && !text.is_empty() {
                out.clear();
            }
        }
        out
    };
    let mut refs = Vec::with_capacity(pieces.len());
    for piece in pieces {
        refs.push(vm.new_string(&piece)?);
    }
    ret_ref(vm.new_ref_array("Ljava/lang/String;", &refs)?)
}

/// `replaceAll`'s replacement: `$n`, `${name}` and `\x` escapes.
fn expand_replacement(vm: &mut Vm, caps: &regex::Captures, replacement: &str, names: &[Option<String>]) -> Res<String> {
    let mut out = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(x) = chars.next() {
                    out.push(x);
                }
            }
            '$' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let name: String = chars.by_ref().take_while(|&x| x != '}').collect();
                    if !names.iter().any(|n| n.as_deref() == Some(&name)) {
                        let msg = format!("No group with name {{{name}}}");
                        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
                    }
                    out.push_str(caps.name(&name).map_or("", |m| m.as_str()));
                } else {
                    let mut n = String::new();
                    while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                        let candidate = format!("{n}{d}");
                        if !n.is_empty() && candidate.parse::<usize>().unwrap_or(usize::MAX) >= caps.len() {
                            break;
                        }
                        n.push(*d);
                        chars.next();
                    }
                    let Ok(i) = n.parse::<usize>() else {
                        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Illegal group reference")));
                    };
                    if i >= caps.len() {
                        let msg = format!("No group {i}");
                        return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
                    }
                    out.push_str(caps.get(i).map_or("", |m| m.as_str()));
                }
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

pub fn regex_replace(vm: &mut Vm, s: ObjId, pattern: ObjId, replacement: ObjId, all: bool) -> Res<Value> {
    let text = String::from_utf16_lossy(&super::string::units(vm, s)?);
    let p = vm.string_nn(pattern)?;
    let rep = vm.string_nn(replacement)?;
    let re = compile_regex(vm, &p, 0)?;
    replace_with(vm, &re, &text, &rep, all)
}

fn replace_with(vm: &mut Vm, re: &Regex, text: &str, rep: &str, all: bool) -> Res<Value> {
    let mut out = String::new();
    let mut last = 0;
    for caps in re.compiled.captures_iter(text) {
        let m = caps.get(0).unwrap();
        out.push_str(&text[last..m.start()]);
        out.push_str(&expand_replacement(vm, &caps, rep, &re.names)?);
        last = m.end();
        if !all {
            break;
        }
    }
    out.push_str(&text[last..]);
    super::ret_str(vm, &out)
}

pub fn regex_matches(vm: &mut Vm, pattern: ObjId, s: ObjId) -> Res<Value> {
    let p = vm.string_nn(pattern)?;
    let text = String::from_utf16_lossy(&super::string::units(vm, s)?);
    let re = compile_regex(vm, &format!("^(?:{p})$"), 0)?;
    ret_bool(re.compiled.is_match(&text))
}

fn pattern_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "compile" => {
            let p = vm.string_nn(arg(a, 0).r())?;
            let flags = if a.len() > 1 { arg(a, 1).i() } else { 0 };
            let re = compile_regex(vm, &p, flags)?;
            let r = new_collection(vm, "java/util/regex/Pattern", State::Regex(Box::new(re)))?;
            ret_ref(r)
        }
        "matches" => {
            let p = arg(a, 0).r();
            regex_matches(vm, p, arg(a, 1).r())
        }
        "quote" => {
            let s = vm.string_nn(arg(a, 0).r())?;
            let q = if s.contains("\\E") { format!("\\Q{}\\E", s.replace("\\E", "\\E\\\\E\\Q")) } else { format!("\\Q{s}\\E") };
            super::ret_str(vm, &q)
        }
        _ => {
            let t = this(vm, a)?;
            let Some(State::Regex(re)) = state(vm, t).cloned() else { return Err(Exc::Internal("not a pattern".into())) };
            match op {
                "matcher" => {
                    let input = super::string::units(vm, arg(a, 1).r())?;
                    let mut m = (*re).clone();
                    m.text = String::from_utf16_lossy(&input);
                    m.input = input;
                    m.pattern = t;
                    ret_ref(new_collection(vm, "java/util/regex/Matcher", State::Regex(Box::new(m)))?)
                }
                "pattern" | "toString" => super::ret_str(vm, &re.source),
                "flags" => Ok(Value::Int(re.flags)),
                "split" => {
                    let s = arg(a, 1).r();
                    let p = vm.new_string(&re.source)?;
                    split(vm, s, p, if a.len() > 2 { arg(a, 2).i() } else { 0 })
                }
                "asPredicate" | "asMatchPredicate" => Err(Exc::Unsupported(format!("Pattern.{op}"))),
                _ => Err(Exc::Unsupported(format!("Pattern.{op}"))),
            }
        }
    }
}

fn matcher_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Regex(mut m)) = state(vm, t).cloned() else { return Err(Exc::Internal("not a matcher".into())) };
    fn spans(caps: Option<regex::Captures>) -> Vec<Option<(usize, usize)>> {
        match caps {
            Some(c) => c.iter().map(|g| g.map(|g| (g.start(), g.end()))).collect(),
            None => Vec::new(),
        }
    }
    let result = match op {
        "matches" | "lookingAt" => {
            let anchored = if op == "matches" { format!("^(?:{})$", translate_pattern(&m.source, m.flags)) } else { format!("^(?:{})", translate_pattern(&m.source, m.flags)) };
            let re = regex::Regex::new(&anchored).map_err(|e| Exc::Internal(e.to_string()))?;
            let groups = spans(re.captures(&m.text));
            let hit = !groups.is_empty();
            m.groups = groups;
            Value::Int(hit as i32)
        }
        "find" | "findFrom" => {
            if op == "findFrom" {
                let start = arg(a, 1).i() as usize;
                m.from = m.text.char_indices().map(|(i, _)| i).chain(std::iter::once(m.text.len())).nth(start).unwrap_or(m.text.len());
            }
            let groups = if m.from <= m.text.len() { spans(m.compiled.captures_at(&m.text, m.from)) } else { Vec::new() };
            let hit = !groups.is_empty();
            match groups.first().copied().flatten() {
                Some((start, end)) => {
                    m.from = if end == start { end + m.text[end..].chars().next().map_or(1, char::len_utf8) } else { end };
                }
                None => m.from = m.text.len() + 1,
            }
            m.groups = groups;
            Value::Int(hit as i32)
        }
        "group" | "groupN" | "groupName" | "start" | "startN" | "end" | "endN" => {
            if m.groups.is_empty() {
                return Err(vm.throw_new("java/lang/IllegalStateException", Some("No match found")));
            }
            let index = match op {
                "groupN" | "startN" | "endN" => arg(a, 1).i() as usize,
                "groupName" => {
                    let name = vm.string_nn(arg(a, 1).r())?;
                    match m.names.iter().position(|n| n.as_deref() == Some(&name)) {
                        Some(i) => i,
                        None => {
                            let msg = format!("No group with name <{name}>");
                            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
                        }
                    }
                }
                _ => 0,
            };
            let Some(span) = m.groups.get(index).copied() else {
                let msg = format!("No group {index}");
                return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
            };
            match (op, span) {
                (o, Some((s, e))) if o.starts_with("group") => Value::Ref(vm.new_string(&m.text[s..e])?),
                (o, None) if o.starts_with("group") => Value::NULL,
                (o, Some((s, _))) if o.starts_with("start") => Value::Int(utf16_index(&m.text, s) as i32),
                (_, Some((_, e))) => Value::Int(utf16_index(&m.text, e) as i32),
                _ => Value::Int(-1),
            }
        }
        "groupCount" => Value::Int(m.compiled.captures_len() as i32 - 1),
        "reset" => {
            m.from = 0;
            m.append_at = 0;
            m.groups.clear();
            Value::Ref(t)
        }
        "replaceAll" | "replaceFirst" => {
            let rep = vm.string_nn(arg(a, 1).r())?;
            let text = m.text.clone();
            return replace_with(vm, &m, &text, &rep, op == "replaceAll");
        }
        "appendReplacement" => {
            let sb = arg(a, 1).r();
            let rep = vm.string_nn(arg(a, 2).r())?;
            let Some(Some((s, e))) = m.groups.first().copied() else {
                return Err(vm.throw_new("java/lang/IllegalStateException", Some("No match available")));
            };
            let caps = m.compiled.captures_at(&m.text, s).unwrap();
            let expanded = expand_replacement(vm, &caps, &rep, &m.names)?;
            let piece = format!("{}{}", &m.text[m.append_at..s], expanded);
            let ps = vm.new_string(&piece)?;
            vm.invoke_virtual(sb, "append", &format!("(Ljava/lang/String;){}", builder_desc(vm, sb)), &[Value::Ref(ps)])?;
            m.append_at = e;
            Value::Ref(t)
        }
        "appendTail" => {
            let sb = arg(a, 1).r();
            let piece = m.text[m.append_at.min(m.text.len())..].to_string();
            let ps = vm.new_string(&piece)?;
            vm.invoke_virtual(sb, "append", &format!("(Ljava/lang/String;){}", builder_desc(vm, sb)), &[Value::Ref(ps)])?;
            Value::Ref(sb)
        }
        "hitEnd" => Value::Int((m.from >= m.text.len()) as i32),
        "results" => {
            let mut items = Vec::new();
            let text = m.text.clone();
            for caps in m.compiled.captures_iter(&text) {
                let g = caps.get(0).unwrap();
                let s = vm.new_string(g.as_str())?;
                items.push(Value::Ref(s));
            }
            return super::stream::stream_of(vm, items);
        }
        "pattern" => Value::Ref(m.pattern),
        _ => return Err(Exc::Unsupported(format!("Matcher.{op}"))),
    };
    if let Some(State::Regex(slot)) = state_mut(vm, t) {
        **slot = *m;
    }
    Ok(result)
}

fn builder_desc(vm: &Vm, sb: ObjId) -> String {
    vm.class_of(sb).map_or("Ljava/lang/StringBuilder;".into(), |c| format!("L{};", vm.class_name(c)))
}

// ------------------------------------------------------------------ StringJoiner

fn joiner_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "init" => {
            let delimiter = super::string::units(vm, arg(a, 1).r())?;
            let (prefix, suffix) = if a.len() > 2 { (super::string::units(vm, arg(a, 2).r())?, super::string::units(vm, arg(a, 3).r())?) } else { (Vec::new(), Vec::new()) };
            if let Some(o) = vm.obj_mut(t) {
                o.data = Data::Jdk(Box::new(State::Joiner(Box::new(Joiner { delimiter, prefix, suffix, parts: None, empty: None }))));
            }
            void()
        }
        "add" => {
            let piece = vm.stringify(arg(a, 1), "Ljava/lang/Object;")?;
            if let Some(State::Joiner(j)) = state_mut(vm, t) {
                match &mut j.parts {
                    Some(p) => {
                        p.extend_from_slice(&j.delimiter.clone());
                        p.extend(piece);
                    }
                    None => j.parts = Some(piece),
                }
            }
            ret_ref(t)
        }
        "setEmptyValue" => {
            let e = super::string::units(vm, arg(a, 1).r())?;
            if let Some(State::Joiner(j)) = state_mut(vm, t) {
                j.empty = Some(e);
            }
            ret_ref(t)
        }
        "toString" | "length" => {
            let Some(State::Joiner(j)) = state(vm, t).cloned() else { return void() };
            let text = match (&j.parts, &j.empty) {
                (None, Some(e)) => e.clone(),
                (p, _) => {
                    let mut out = j.prefix.clone();
                    out.extend(p.clone().unwrap_or_default());
                    out.extend(&j.suffix);
                    out
                }
            };
            if op == "length" {
                return Ok(Value::Int(text.len() as i32));
            }
            ret_ref(vm.new_string_units(text)?)
        }
        _ => Err(Exc::Unsupported(format!("StringJoiner.{op}"))),
    }
}

// ------------------------------------------------------------------ atomics

fn atomic_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let current = match state(vm, t) {
        Some(State::Atomic(v)) => *v,
        _ => Value::Int(0),
    };
    let set = |vm: &mut Vm, v: Value| {
        if let Some(o) = vm.obj_mut(t) {
            o.data = Data::Jdk(Box::new(State::Atomic(v)));
        }
    };
    let add = |v: Value, d: i64| match v {
        Value::Long(l) => Value::Long(l.wrapping_add(d)),
        v => Value::Int(v.i().wrapping_add(d as i32)),
    };
    let delta = |v: Value| match v {
        Value::Long(l) => l,
        v => v.i() as i64,
    };
    Ok(match op {
        "init" => {
            set(vm, a.get(1).copied().unwrap_or(Value::Int(0)));
            Value::Top
        }
        "initLong" => {
            set(vm, a.get(1).copied().unwrap_or(Value::Long(0)));
            Value::Top
        }
        "initRef" => {
            set(vm, a.get(1).copied().unwrap_or(Value::NULL));
            Value::Top
        }
        "get" => current,
        "set" => {
            set(vm, arg(a, 1));
            Value::Top
        }
        "getAndSet" => {
            set(vm, arg(a, 1));
            current
        }
        "incrementAndGet" => {
            let v = add(current, 1);
            set(vm, v);
            v
        }
        "decrementAndGet" => {
            let v = add(current, -1);
            set(vm, v);
            v
        }
        "getAndIncrement" => {
            set(vm, add(current, 1));
            current
        }
        "getAndDecrement" => {
            set(vm, add(current, -1));
            current
        }
        "addAndGet" => {
            let v = add(current, delta(arg(a, 1)));
            set(vm, v);
            v
        }
        "getAndAdd" => {
            set(vm, add(current, delta(arg(a, 1))));
            current
        }
        "compareAndSet" | "compareAndExchange" => {
            let same = match (current, arg(a, 1)) {
                (Value::Ref(x), Value::Ref(y)) => x == y,
                (x, y) => x == y,
            };
            if same {
                set(vm, arg(a, 2));
            }
            if op == "compareAndExchange" { current } else { Value::Int(same as i32) }
        }
        "updateAndGet" | "getAndUpdate" => {
            let f = arg(a, 1).r();
            let (name, desc) = match current {
                Value::Long(_) => ("applyAsLong", "(J)J"),
                Value::Ref(_) => ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                _ => ("applyAsInt", "(I)I"),
            };
            let v = vm.invoke_virtual(f, name, desc, &[current])?;
            set(vm, v);
            if op == "updateAndGet" { v } else { current }
        }
        "accumulateAndGet" | "getAndAccumulate" => {
            let f = arg(a, 2).r();
            let (name, desc) = match current {
                Value::Long(_) => ("applyAsLong", "(JJ)J"),
                Value::Ref(_) => ("apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
                _ => ("applyAsInt", "(II)I"),
            };
            let v = vm.invoke_virtual(f, name, desc, &[current, arg(a, 1)])?;
            set(vm, v);
            if op == "accumulateAndGet" { v } else { current }
        }
        "intValue" => Value::Int(delta(current) as i32),
        "longValue" => Value::Long(delta(current)),
        "doubleValue" => Value::Double(delta(current) as f64),
        "toString" => {
            let desc = match current {
                Value::Long(_) => "J",
                Value::Ref(_) => "Ljava/lang/Object;",
                _ => "I",
            };
            let desc = if vm.class_of(t).map(|c| vm.class_name(c) == "java/util/concurrent/atomic/AtomicBoolean").unwrap_or(false) { "Z" } else { desc };
            let u = vm.stringify(current, desc)?;
            Value::Ref(vm.new_string_units(u)?)
        }
        _ => return Err(Exc::Unsupported(format!("atomic {op}"))),
    })
}

// ------------------------------------------------------------------ classes

macro_rules! list_methods {
    () => {
        natives![
            "<init>" "()V" M => Some(list_init),
            "<init>" "(I)V" M => Some(list_init),
            "<init>" "(Ljava/util/Collection;)V" M => Some(list_init),
            "size" "()I" M => Some(|vm, a| list_op(vm, a, "size")),
            "isEmpty" "()Z" M => Some(|vm, a| list_op(vm, a, "isEmpty")),
            "get" "(I)Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "get")),
            "set" "(ILjava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "set")),
            "add" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "add")),
            "add" "(ILjava/lang/Object;)V" M => Some(|vm, a| list_op(vm, a, "addAt")),
            "remove" "(I)Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "removeAt")),
            "remove" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "remove")),
            "remove" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "remove0")),
            "indexOf" "(Ljava/lang/Object;)I" M => Some(|vm, a| list_op(vm, a, "indexOf")),
            "lastIndexOf" "(Ljava/lang/Object;)I" M => Some(|vm, a| list_op(vm, a, "lastIndexOf")),
            "contains" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "contains")),
            "containsAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| list_op(vm, a, "containsAll")),
            "clear" "()V" M => Some(|vm, a| list_op(vm, a, "clear")),
            "addAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| list_op(vm, a, "addAll")),
            "addAll" "(ILjava/util/Collection;)Z" M => Some(|vm, a| list_op(vm, a, "addAllAt")),
            "removeAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| list_op(vm, a, "removeAll")),
            "retainAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| list_op(vm, a, "retainAll")),
            "iterator" "()Ljava/util/Iterator;" M => Some(|vm, a| list_op(vm, a, "iterator")),
            "listIterator" "()Ljava/util/ListIterator;" M => Some(|vm, a| list_op(vm, a, "listIterator")),
            "listIterator" "(I)Ljava/util/ListIterator;" M => Some(|vm, a| list_op(vm, a, "listIteratorAt")),
            "descendingIterator" "()Ljava/util/Iterator;" M => Some(|vm, a| list_op(vm, a, "descendingIterator")),
            "toArray" "()[Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "toArray")),
            "toArray" "([Ljava/lang/Object;)[Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "toArrayTyped")),
            "subList" "(II)Ljava/util/List;" M => Some(|vm, a| list_op(vm, a, "subList")),
            "sort" "(Ljava/util/Comparator;)V" M => Some(|vm, a| list_op(vm, a, "sort")),
            "replaceAll" "(Ljava/util/function/UnaryOperator;)V" M => Some(|vm, a| list_op(vm, a, "replaceAll")),
            "removeIf" "(Ljava/util/function/Predicate;)Z" M => Some(|vm, a| list_op(vm, a, "removeIf")),
            "forEach" "(Ljava/util/function/Consumer;)V" M => Some(|vm, a| list_op(vm, a, "forEach")),
            "hashCode" "()I" M => Some(|vm, a| list_op(vm, a, "hashCode")),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "equals")),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| list_op(vm, a, "toString")),
            "stream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| list_op(vm, a, "stream")),
            "getFirst" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "getFirst")),
            "getLast" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "getLast")),
            "removeFirst" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "removeFirst")),
            "removeLast" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "removeLast")),
            "addFirst" "(Ljava/lang/Object;)V" M => Some(|vm, a| list_op(vm, a, "addFirst")),
            "addLast" "(Ljava/lang/Object;)V" M => Some(|vm, a| list_op(vm, a, "addLast")),
            "offerFirst" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "offerFirst")),
            "offerLast" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "offerLast")),
            "offer" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "offer")),
            "push" "(Ljava/lang/Object;)V" M => Some(|vm, a| list_op(vm, a, "push")),
            "pop" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "pop")),
            "poll" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "poll")),
            "pollFirst" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "pollFirst")),
            "pollLast" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "pollLast")),
            "peek" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "peek")),
            "peekFirst" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "peekFirst")),
            "peekLast" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "peekLast")),
            "element" "()Ljava/lang/Object;" M => Some(|vm, a| list_op(vm, a, "element")),
            "removeFirstOccurrence" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "removeFirstOccurrence")),
            "removeLastOccurrence" "(Ljava/lang/Object;)Z" M => Some(|vm, a| list_op(vm, a, "removeLastOccurrence")),
            "reversed" "()Ljava/util/List;" M => Some(|vm, a| list_op(vm, a, "reversed")),
            "ensureCapacity" "(I)V" M => Some(|_, _| void()),
            "trimToSize" "()V" M => Some(|_, _| void()),
            "clone" "()Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let items = list_items(vm, t)?; let c = vm.class_of(a[0].r()).unwrap(); let r = vm.new_instance(c)?; if let Some(o) = vm.obj_mut(r) { o.data = Data::Jdk(Box::new(State::List(JList { items, ..Default::default() }))); } ret_ref(r) }),
        ]
    };
}

macro_rules! map_methods {
    ($init:expr) => {
        natives![
            "<init>" "()V" M => Some($init),
            "<init>" "(I)V" M => Some($init),
            "<init>" "(IF)V" M => Some($init),
            "<init>" "(IFZ)V" M => Some($init),
            "<init>" "(Ljava/util/Map;)V" M => Some($init),
            "size" "()I" M => Some(|vm, a| map_op(vm, a, "size")),
            "isEmpty" "()Z" M => Some(|vm, a| map_op(vm, a, "isEmpty")),
            "get" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "get")),
            "getOrDefault" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "getOrDefault")),
            "containsKey" "(Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "containsKey")),
            "containsValue" "(Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "containsValue")),
            "put" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "put")),
            "putIfAbsent" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "putIfAbsent")),
            "remove" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "remove")),
            "remove" "(Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "removePair")),
            "replace" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "replace")),
            "replace" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "replaceIf")),
            "clear" "()V" M => Some(|vm, a| map_op(vm, a, "clear")),
            "putAll" "(Ljava/util/Map;)V" M => Some(|vm, a| map_op(vm, a, "putAll")),
            "keySet" "()Ljava/util/Set;" M => Some(|vm, a| map_op(vm, a, "keySet")),
            "values" "()Ljava/util/Collection;" M => Some(|vm, a| map_op(vm, a, "values")),
            "entrySet" "()Ljava/util/Set;" M => Some(|vm, a| map_op(vm, a, "entrySet")),
            "forEach" "(Ljava/util/function/BiConsumer;)V" M => Some(|vm, a| map_op(vm, a, "forEach")),
            "replaceAll" "(Ljava/util/function/BiFunction;)V" M => Some(|vm, a| map_op(vm, a, "replaceAll")),
            "computeIfAbsent" "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "computeIfAbsent")),
            "computeIfPresent" "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "computeIfPresent")),
            "compute" "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "compute")),
            "merge" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "merge")),
            "hashCode" "()I" M => Some(|vm, a| map_op(vm, a, "hashCode")),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "equals")),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| map_op(vm, a, "toString")),
            "removeEldestEntry" "(Ljava/util/Map$Entry;)Z" 0x0004 => Some(|vm, a| map_op(vm, a, "removeEldestEntry")),
            "firstEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "firstEntry")),
            "lastEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "lastEntry")),
            "pollFirstEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "pollFirstEntry")),
            "pollLastEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "pollLastEntry")),
            "clone" "()Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let c = vm.class_of(t).unwrap(); let r = vm.new_instance(c)?; let data = vm.obj(t).unwrap().data.clone(); if let Some(o) = vm.obj_mut(r) { o.data = data; } ret_ref(r) }),
        ]
    };
}

macro_rules! set_methods {
    ($init:expr) => {
        natives![
            "<init>" "()V" M => Some($init),
            "<init>" "(I)V" M => Some($init),
            "<init>" "(IF)V" M => Some($init),
            "<init>" "(Ljava/util/Collection;)V" M => Some($init),
            "add" "(Ljava/lang/Object;)Z" M => Some(|vm, a| set_op(vm, a, "add")),
            "remove" "(Ljava/lang/Object;)Z" M => Some(|vm, a| set_op(vm, a, "remove")),
            "contains" "(Ljava/lang/Object;)Z" M => Some(|vm, a| set_op(vm, a, "contains")),
            "size" "()I" M => Some(|vm, a| set_op(vm, a, "size")),
            "isEmpty" "()Z" M => Some(|vm, a| set_op(vm, a, "isEmpty")),
            "clear" "()V" M => Some(|vm, a| set_op(vm, a, "clear")),
            "iterator" "()Ljava/util/Iterator;" M => Some(|vm, a| set_op(vm, a, "iterator")),
            "addAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| set_op(vm, a, "addAll")),
            "removeAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| set_op(vm, a, "removeAll")),
            "retainAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| set_op(vm, a, "retainAll")),
            "removeIf" "(Ljava/util/function/Predicate;)Z" M => Some(|vm, a| set_op(vm, a, "removeIf")),
            "containsAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| set_op(vm, a, "containsAll")),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| set_op(vm, a, "toString")),
            "hashCode" "()I" M => Some(|vm, a| set_op(vm, a, "hashCode")),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| set_op(vm, a, "equals")),
            "toArray" "()[Ljava/lang/Object;" M => Some(|vm, a| set_op(vm, a, "toArray")),
            "toArray" "([Ljava/lang/Object;)[Ljava/lang/Object;" M => Some(|vm, a| set_op(vm, a, "toArrayTyped")),
            "stream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| set_op(vm, a, "stream")),
            "forEach" "(Ljava/util/function/Consumer;)V" M => Some(|vm, a| set_op(vm, a, "forEach")),
            "getFirst" "()Ljava/lang/Object;" M => Some(|vm, a| set_op(vm, a, "getFirst")),
            "getLast" "()Ljava/lang/Object;" M => Some(|vm, a| set_op(vm, a, "getLast")),
            "removeFirst" "()Ljava/lang/Object;" M => Some(|vm, a| set_op(vm, a, "removeFirst")),
            "removeLast" "()Ljava/lang/Object;" M => Some(|vm, a| set_op(vm, a, "removeLast")),
            "reversed" "()Ljava/util/SequencedSet;" M => Some(|vm, a| set_op(vm, a, "reversed")),
            "first" "()Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "first")),
            "last" "()Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "last")),
            "pollFirst" "()Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "pollFirst")),
            "pollLast" "()Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "pollLast")),
            "floor" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "floor")),
            "ceiling" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "ceiling")),
            "higher" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "higher")),
            "lower" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_set_op(vm, a, "lower")),
            "headSet" "(Ljava/lang/Object;)Ljava/util/SortedSet;" M => Some(|vm, a| tree_set_op(vm, a, "headSet")),
            "headSet" "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;" M => Some(|vm, a| tree_set_op(vm, a, "headSetIncl")),
            "tailSet" "(Ljava/lang/Object;)Ljava/util/SortedSet;" M => Some(|vm, a| tree_set_op(vm, a, "tailSet")),
            "tailSet" "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;" M => Some(|vm, a| tree_set_op(vm, a, "tailSetIncl")),
            "subSet" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;" M => Some(|vm, a| tree_set_op(vm, a, "subSet")),
            "descendingSet" "()Ljava/util/NavigableSet;" M => Some(|vm, a| tree_set_op(vm, a, "descendingSet")),
            "descendingIterator" "()Ljava/util/Iterator;" M => Some(|vm, a| tree_set_op(vm, a, "descendingIterator")),
            "comparator" "()Ljava/util/Comparator;" M => Some(|vm, a| tree_set_op(vm, a, "comparator")),
            "clone" "()Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let c = vm.class_of(t).unwrap(); let r = vm.new_instance(c)?; let data = vm.obj(t).unwrap().data.clone(); if let Some(o) = vm.obj_mut(r) { o.data = data; } ret_ref(r) }),
        ]
    };
}

static LIST_METHODS: &[NativeMethod] = list_methods!();
static HASHMAP_METHODS: &[NativeMethod] = map_methods!(|vm, a| map_init(vm, a, false, false));
static LINKEDHASHMAP_METHODS: &[NativeMethod] = map_methods!(|vm, a| map_init(vm, a, true, false));
static IDENTITYMAP_METHODS: &[NativeMethod] = map_methods!(|vm, a| map_init(vm, a, false, true));
static HASHSET_METHODS: &[NativeMethod] = set_methods!(|vm, a| set_init(vm, a, false));
static LINKEDHASHSET_METHODS: &[NativeMethod] = set_methods!(|vm, a| set_init(vm, a, true));
static TREESET_METHODS: &[NativeMethod] = set_methods!(|vm, a| tree_init(vm, a, true));

fn list_data() -> Data {
    Data::Jdk(Box::new(State::List(JList::default())))
}

fn map_data() -> Data {
    Data::Jdk(Box::new(State::Map(Box::new(JMap::new()))))
}

const fn collection(name: &'static str, super_name: &'static str, interfaces: &'static [&'static str], methods: &'static [NativeMethod], data: fn() -> Data) -> NativeClass {
    NativeClass { name, super_name: Some(super_name), interfaces, access: PUBLIC, methods, statics: &[], clinit: None, data: Some(data) }
}

static ITER_METHODS: &[NativeMethod] = natives![
    "hasNext" "()Z" M => Some(iter_has_next),
    "next" "()Ljava/lang/Object;" M => Some(iter_next),
    "remove" "()V" M => Some(iter_remove),
    "hasPrevious" "()Z" M => Some(|vm, a| list_iter_op(vm, a, "hasPrevious")),
    "previous" "()Ljava/lang/Object;" M => Some(|vm, a| list_iter_op(vm, a, "previous")),
    "nextIndex" "()I" M => Some(|vm, a| list_iter_op(vm, a, "nextIndex")),
    "previousIndex" "()I" M => Some(|vm, a| list_iter_op(vm, a, "previousIndex")),
    "set" "(Ljava/lang/Object;)V" M => Some(|vm, a| list_iter_op(vm, a, "set")),
    "add" "(Ljava/lang/Object;)V" M => Some(|vm, a| list_iter_op(vm, a, "add")),
    "forEachRemaining" "(Ljava/util/function/Consumer;)V" M => Some(|vm, a| { let t = this(vm, a)?; while iter_has_next(vm, a)?.z() { let v = iter_next(vm, &[Value::Ref(t)])?; vm.invoke_virtual(arg(a, 1).r(), "accept", "(Ljava/lang/Object;)V", &[v])?; } void() }),
];

static VIEW_METHODS: &[NativeMethod] = natives![
    "size" "()I" M => Some(|vm, a| view_op(vm, a, "size")),
    "isEmpty" "()Z" M => Some(|vm, a| view_op(vm, a, "isEmpty")),
    "iterator" "()Ljava/util/Iterator;" M => Some(|vm, a| view_op(vm, a, "iterator")),
    "contains" "(Ljava/lang/Object;)Z" M => Some(|vm, a| view_op(vm, a, "contains")),
    "containsAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| view_op(vm, a, "containsAll")),
    "remove" "(Ljava/lang/Object;)Z" M => Some(|vm, a| view_op(vm, a, "remove")),
    "clear" "()V" M => Some(|vm, a| view_op(vm, a, "clear")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| view_op(vm, a, "toString")),
    "toArray" "()[Ljava/lang/Object;" M => Some(|vm, a| view_op(vm, a, "toArray")),
    "toArray" "([Ljava/lang/Object;)[Ljava/lang/Object;" M => Some(|vm, a| view_op(vm, a, "toArrayTyped")),
    "stream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| view_op(vm, a, "stream")),
    "forEach" "(Ljava/util/function/Consumer;)V" M => Some(|vm, a| view_op(vm, a, "forEach")),
    "removeIf" "(Ljava/util/function/Predicate;)Z" M => Some(|vm, a| view_op(vm, a, "removeIf")),
    "hashCode" "()I" M => Some(|vm, a| view_op(vm, a, "hashCode")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| view_op(vm, a, "equals")),
];

static ENTRY_METHODS: &[NativeMethod] = natives![
    "<init>" "(Ljava/lang/Object;Ljava/lang/Object;)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Jdk(Box::new(State::Entry { key: arg(a, 1), value: arg(a, 2), map: 0 })); } void() }),
    "getKey" "()Ljava/lang/Object;" M => Some(|vm, a| entry_op(vm, a, "getKey")),
    "getValue" "()Ljava/lang/Object;" M => Some(|vm, a| entry_op(vm, a, "getValue")),
    "setValue" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| entry_op(vm, a, "setValue")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| entry_op(vm, a, "toString")),
    "hashCode" "()I" M => Some(|vm, a| entry_op(vm, a, "hashCode")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| entry_op(vm, a, "equals")),
];

static TREEMAP_METHODS: &[NativeMethod] = natives![
    "<init>" "()V" M => Some(|vm, a| tree_init(vm, a, false)),
    "<init>" "(Ljava/util/Comparator;)V" M => Some(|vm, a| tree_init(vm, a, false)),
    "<init>" "(Ljava/util/Map;)V" M => Some(|vm, a| tree_init(vm, a, false)),
    "<init>" "(Ljava/util/SortedMap;)V" M => Some(|vm, a| tree_init(vm, a, false)),
    "size" "()I" M => Some(|vm, a| map_op(vm, a, "size")),
    "isEmpty" "()Z" M => Some(|vm, a| map_op(vm, a, "isEmpty")),
    "get" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "get")),
    "getOrDefault" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "getOrDefault")),
    "containsKey" "(Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "containsKey")),
    "containsValue" "(Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "containsValue")),
    "put" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "put")),
    "putIfAbsent" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "putIfAbsent")),
    "remove" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "remove")),
    "remove" "(Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "removePair")),
    "replace" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "replace")),
    "replace" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "replaceIf")),
    "replaceAll" "(Ljava/util/function/BiFunction;)V" M => Some(|vm, a| map_op(vm, a, "replaceAll")),
    "clear" "()V" M => Some(|vm, a| map_op(vm, a, "clear")),
    "putAll" "(Ljava/util/Map;)V" M => Some(|vm, a| map_op(vm, a, "putAll")),
    "keySet" "()Ljava/util/Set;" M => Some(|vm, a| map_op(vm, a, "keySet")),
    "values" "()Ljava/util/Collection;" M => Some(|vm, a| map_op(vm, a, "values")),
    "entrySet" "()Ljava/util/Set;" M => Some(|vm, a| map_op(vm, a, "entrySet")),
    "forEach" "(Ljava/util/function/BiConsumer;)V" M => Some(|vm, a| map_op(vm, a, "forEach")),
    "computeIfAbsent" "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "computeIfAbsent")),
    "computeIfPresent" "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "computeIfPresent")),
    "compute" "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "compute")),
    "merge" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "merge")),
    "hashCode" "()I" M => Some(|vm, a| map_op(vm, a, "hashCode")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| map_op(vm, a, "equals")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| map_op(vm, a, "toString")),
    "firstKey" "()Ljava/lang/Object;" M => Some(|vm, a| tree_map_op(vm, a, "firstKey")),
    "lastKey" "()Ljava/lang/Object;" M => Some(|vm, a| tree_map_op(vm, a, "lastKey")),
    "firstEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "firstEntry")),
    "lastEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "lastEntry")),
    "pollFirstEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "pollFirstEntry")),
    "pollLastEntry" "()Ljava/util/Map$Entry;" M => Some(|vm, a| map_op(vm, a, "pollLastEntry")),
    "floorKey" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_map_op(vm, a, "floorKey")),
    "ceilingKey" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_map_op(vm, a, "ceilingKey")),
    "higherKey" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_map_op(vm, a, "higherKey")),
    "lowerKey" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| tree_map_op(vm, a, "lowerKey")),
    "floorEntry" "(Ljava/lang/Object;)Ljava/util/Map$Entry;" M => Some(|vm, a| tree_map_op(vm, a, "floorEntry")),
    "ceilingEntry" "(Ljava/lang/Object;)Ljava/util/Map$Entry;" M => Some(|vm, a| tree_map_op(vm, a, "ceilingEntry")),
    "higherEntry" "(Ljava/lang/Object;)Ljava/util/Map$Entry;" M => Some(|vm, a| tree_map_op(vm, a, "higherEntry")),
    "lowerEntry" "(Ljava/lang/Object;)Ljava/util/Map$Entry;" M => Some(|vm, a| tree_map_op(vm, a, "lowerEntry")),
    "headMap" "(Ljava/lang/Object;)Ljava/util/SortedMap;" M => Some(|vm, a| tree_map_op(vm, a, "headMap")),
    "headMap" "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;" M => Some(|vm, a| tree_map_op(vm, a, "headMapIncl")),
    "tailMap" "(Ljava/lang/Object;)Ljava/util/SortedMap;" M => Some(|vm, a| tree_map_op(vm, a, "tailMap")),
    "tailMap" "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;" M => Some(|vm, a| tree_map_op(vm, a, "tailMapIncl")),
    "subMap" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;" M => Some(|vm, a| tree_map_op(vm, a, "subMap")),
    "subMap" "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;" M => Some(|vm, a| tree_map_op(vm, a, "subMapIncl")),
    "descendingMap" "()Ljava/util/NavigableMap;" M => Some(|vm, a| tree_map_op(vm, a, "descendingMap")),
    "navigableKeySet" "()Ljava/util/NavigableSet;" M => Some(|vm, a| tree_map_op(vm, a, "navigableKeySet")),
    "descendingKeySet" "()Ljava/util/NavigableSet;" M => Some(|vm, a| tree_map_op(vm, a, "descendingKeySet")),
    "comparator" "()Ljava/util/Comparator;" M => Some(|vm, a| tree_map_op(vm, a, "comparator")),
];

static HEAP_METHODS: &[NativeMethod] = natives![
    "<init>" "()V" M => Some(heap_init),
    "<init>" "(I)V" M => Some(heap_init),
    "<init>" "(Ljava/util/Comparator;)V" M => Some(heap_init),
    "<init>" "(ILjava/util/Comparator;)V" M => Some(heap_init),
    "<init>" "(Ljava/util/Collection;)V" M => Some(heap_init),
    "add" "(Ljava/lang/Object;)Z" M => Some(|vm, a| heap_op(vm, a, "add")),
    "offer" "(Ljava/lang/Object;)Z" M => Some(|vm, a| heap_op(vm, a, "offer")),
    "poll" "()Ljava/lang/Object;" M => Some(|vm, a| heap_op(vm, a, "poll")),
    "remove" "()Ljava/lang/Object;" M => Some(|vm, a| heap_op(vm, a, "remove0")),
    "remove" "(Ljava/lang/Object;)Z" M => Some(|vm, a| heap_op(vm, a, "remove")),
    "peek" "()Ljava/lang/Object;" M => Some(|vm, a| heap_op(vm, a, "peek")),
    "element" "()Ljava/lang/Object;" M => Some(|vm, a| heap_op(vm, a, "element")),
    "size" "()I" M => Some(|vm, a| heap_op(vm, a, "size")),
    "isEmpty" "()Z" M => Some(|vm, a| heap_op(vm, a, "isEmpty")),
    "clear" "()V" M => Some(|vm, a| heap_op(vm, a, "clear")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| heap_op(vm, a, "toString")),
    "iterator" "()Ljava/util/Iterator;" M => Some(|vm, a| heap_op(vm, a, "iterator")),
    "contains" "(Ljava/lang/Object;)Z" M => Some(|vm, a| heap_op(vm, a, "contains")),
    "toArray" "()[Ljava/lang/Object;" M => Some(|vm, a| heap_op(vm, a, "toArray")),
    "comparator" "()Ljava/util/Comparator;" M => Some(|vm, a| heap_op(vm, a, "comparator")),
    "stream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| heap_op(vm, a, "stream")),
    "addAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| heap_op(vm, a, "addAll")),
];

static OPTIONAL_METHODS: &[NativeMethod] = natives![
    "of" "(Ljava/lang/Object;)Ljava/util/Optional;" S => Some(|vm, a| { if arg(a, 0).r() == 0 { return Err(vm.npe(None)); } optional_of(vm, arg(a, 0)) }),
    "ofNullable" "(Ljava/lang/Object;)Ljava/util/Optional;" S => Some(|vm, a| optional_of(vm, arg(a, 0))),
    "empty" "()Ljava/util/Optional;" S => Some(|vm, _| optional_of(vm, Value::NULL)),
    "isPresent" "()Z" M => Some(|vm, a| optional_op(vm, a, "isPresent", b'L')),
    "isEmpty" "()Z" M => Some(|vm, a| optional_op(vm, a, "isEmpty", b'L')),
    "get" "()Ljava/lang/Object;" M => Some(|vm, a| optional_op(vm, a, "get", b'L')),
    "orElseThrow" "()Ljava/lang/Object;" M => Some(|vm, a| optional_op(vm, a, "orElseThrow", b'L')),
    "orElseThrow" "(Ljava/util/function/Supplier;)Ljava/lang/Object;" M => Some(|vm, a| optional_op(vm, a, "orElseThrowSupplier", b'L')),
    "orElse" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| optional_op(vm, a, "orElse", b'L')),
    "orElseGet" "(Ljava/util/function/Supplier;)Ljava/lang/Object;" M => Some(|vm, a| optional_op(vm, a, "orElseGet", b'L')),
    "ifPresent" "(Ljava/util/function/Consumer;)V" M => Some(|vm, a| optional_op(vm, a, "ifPresent", b'L')),
    "ifPresentOrElse" "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V" M => Some(|vm, a| optional_op(vm, a, "ifPresentOrElse", b'L')),
    "map" "(Ljava/util/function/Function;)Ljava/util/Optional;" M => Some(|vm, a| optional_op(vm, a, "map", b'L')),
    "flatMap" "(Ljava/util/function/Function;)Ljava/util/Optional;" M => Some(|vm, a| optional_op(vm, a, "flatMap", b'L')),
    "filter" "(Ljava/util/function/Predicate;)Ljava/util/Optional;" M => Some(|vm, a| optional_op(vm, a, "filter", b'L')),
    "or" "(Ljava/util/function/Supplier;)Ljava/util/Optional;" M => Some(|vm, a| optional_op(vm, a, "or", b'L')),
    "stream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| optional_op(vm, a, "stream", b'L')),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| optional_op(vm, a, "equals", b'L')),
    "hashCode" "()I" M => Some(|vm, a| optional_op(vm, a, "hashCode", b'L')),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| optional_op(vm, a, "toString", b'L')),
];

macro_rules! primitive_optional {
    ($class:literal, $p:literal, $kind:expr, $get:literal, $sup:literal) => {
        natives![
            "of" (concat!("(", $p, ")L", $class, ";")) S => Some(|vm, a| new_optional(vm, $class, arg(a, 0), true)),
            "empty" (concat!("()L", $class, ";")) S => Some(|vm, _| new_optional(vm, $class, Value::zero($p), false)),
            "isPresent" "()Z" M => Some(|vm, a| optional_op(vm, a, "isPresent", $kind)),
            "isEmpty" "()Z" M => Some(|vm, a| optional_op(vm, a, "isEmpty", $kind)),
            $get (concat!("()", $p)) M => Some(|vm, a| optional_op(vm, a, "get", $kind)),
            "orElseThrow" (concat!("()", $p)) M => Some(|vm, a| optional_op(vm, a, "get", $kind)),
            "orElse" (concat!("(", $p, ")", $p)) M => Some(|vm, a| optional_op(vm, a, "orElse", $kind)),
            "orElseGet" (concat!("(L", $sup, ";)", $p)) M => Some(|vm, a| optional_op(vm, a, "orElseGet", $kind)),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| optional_op(vm, a, "equals", $kind)),
            "hashCode" "()I" M => Some(|vm, a| optional_op(vm, a, "hashCode", $kind)),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| optional_op(vm, a, "toString", $kind)),
        ]
    };
}

static RANDOM_METHODS: &[NativeMethod] = natives![
    "<init>" "()V" M => Some(|vm, a| random_op(vm, a, "init")),
    "<init>" "(J)V" M => Some(|vm, a| random_op(vm, a, "init")),
    "setSeed" "(J)V" M => Some(|vm, a| random_op(vm, a, "setSeed")),
    "next" "(I)I" 0x0004 => Some(|vm, a| random_op(vm, a, "next")),
    "nextInt" "()I" M => Some(|vm, a| random_op(vm, a, "nextInt")),
    "nextInt" "(I)I" M => Some(|vm, a| random_op(vm, a, "nextIntBound")),
    "nextInt" "(II)I" M => Some(|vm, a| random_op(vm, a, "nextIntRange")),
    "nextLong" "()J" M => Some(|vm, a| random_op(vm, a, "nextLong")),
    "nextLong" "(J)J" M => Some(|vm, a| random_op(vm, a, "nextLongBound")),
    "nextDouble" "()D" M => Some(|vm, a| random_op(vm, a, "nextDouble")),
    "nextDouble" "(DD)D" M => Some(|vm, a| random_op(vm, a, "nextDoubleRange")),
    "nextFloat" "()F" M => Some(|vm, a| random_op(vm, a, "nextFloat")),
    "nextBoolean" "()Z" M => Some(|vm, a| random_op(vm, a, "nextBoolean")),
    "nextGaussian" "()D" M => Some(|vm, a| random_op(vm, a, "nextGaussian")),
    "nextBytes" "([B)V" M => Some(|vm, a| random_op(vm, a, "nextBytes")),
    "ints" "(J)Ljava/util/stream/IntStream;" M => Some(|vm, a| random_op(vm, a, "ints")),
    "ints" "(JII)Ljava/util/stream/IntStream;" M => Some(|vm, a| random_op(vm, a, "ints")),
    "doubles" "(J)Ljava/util/stream/DoubleStream;" M => Some(|vm, a| random_op(vm, a, "doubles")),
];

macro_rules! atomic_class {
    ($name:literal, $p:literal, $init:literal) => {
        NativeClass {
            name: $name,
            super_name: Some("java/lang/Number"),
            interfaces: &["java/io/Serializable"],
            access: PUBLIC,
            methods: natives![
                "<init>" "()V" M => Some(|vm, a| atomic_op(vm, a, $init)),
                "<init>" (concat!("(", $p, ")V")) M => Some(|vm, a| atomic_op(vm, a, $init)),
                "get" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "get")),
                "getPlain" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "get")),
                "getAcquire" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "get")),
                "set" (concat!("(", $p, ")V")) M => Some(|vm, a| atomic_op(vm, a, "set")),
                "lazySet" (concat!("(", $p, ")V")) M => Some(|vm, a| atomic_op(vm, a, "set")),
                "setPlain" (concat!("(", $p, ")V")) M => Some(|vm, a| atomic_op(vm, a, "set")),
                "setRelease" (concat!("(", $p, ")V")) M => Some(|vm, a| atomic_op(vm, a, "set")),
                "getAndSet" (concat!("(", $p, ")", $p)) M => Some(|vm, a| atomic_op(vm, a, "getAndSet")),
                "incrementAndGet" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "incrementAndGet")),
                "decrementAndGet" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "decrementAndGet")),
                "getAndIncrement" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "getAndIncrement")),
                "getAndDecrement" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "getAndDecrement")),
                "addAndGet" (concat!("(", $p, ")", $p)) M => Some(|vm, a| atomic_op(vm, a, "addAndGet")),
                "getAndAdd" (concat!("(", $p, ")", $p)) M => Some(|vm, a| atomic_op(vm, a, "getAndAdd")),
                "compareAndSet" (concat!("(", $p, $p, ")Z")) M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
                "weakCompareAndSet" (concat!("(", $p, $p, ")Z")) M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
                "weakCompareAndSetPlain" (concat!("(", $p, $p, ")Z")) M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
                "weakCompareAndSetVolatile" (concat!("(", $p, $p, ")Z")) M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
                "weakCompareAndSetAcquire" (concat!("(", $p, $p, ")Z")) M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
                "weakCompareAndSetRelease" (concat!("(", $p, $p, ")Z")) M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
                "compareAndExchange" (concat!("(", $p, $p, ")", $p)) M => Some(|vm, a| atomic_op(vm, a, "compareAndExchange")),
                "compareAndExchangeAcquire" (concat!("(", $p, $p, ")", $p)) M => Some(|vm, a| atomic_op(vm, a, "compareAndExchange")),
                "compareAndExchangeRelease" (concat!("(", $p, $p, ")", $p)) M => Some(|vm, a| atomic_op(vm, a, "compareAndExchange")),
                "getOpaque" (concat!("()", $p)) M => Some(|vm, a| atomic_op(vm, a, "get")),
                "setOpaque" (concat!("(", $p, ")V")) M => Some(|vm, a| atomic_op(vm, a, "set")),
                "intValue" "()I" M => Some(|vm, a| atomic_op(vm, a, "intValue")),
                "longValue" "()J" M => Some(|vm, a| atomic_op(vm, a, "longValue")),
                "doubleValue" "()D" M => Some(|vm, a| atomic_op(vm, a, "doubleValue")),
                "floatValue" "()F" M => Some(|vm, a| { let d = atomic_op(vm, a, "doubleValue")?.d(); Ok(Value::Float(d as f32)) }),
                "toString" "()Ljava/lang/String;" M => Some(|vm, a| atomic_op(vm, a, "toString")),
            ],
            statics: &[],
            clinit: None,
            data: Some(|| Data::Jdk(Box::new(State::Atomic(Value::zero($p))))),
        }
    };
}

static CLASSES: &[NativeClass] = &[
    class("java/util/Collection", None, &["java/lang/Iterable"], INTERFACE, natives![
        "size" "()I" A => None,
        "isEmpty" "()Z" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.invoke_virtual(t, "size", "()I", &[])?; ret_bool(n.i() == 0) }),
        "contains" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; for v in iterate(vm, t)? { if equals(vm, arg(a, 1), v)? { return ret_bool(true); } } ret_bool(false) }),
        "add" "(Ljava/lang/Object;)Z" M => Some(|vm, _| Err(uoe(vm))),
        "remove" "(Ljava/lang/Object;)Z" M => Some(|vm, _| Err(uoe(vm))),
        "addAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let mut c = false; for v in iterate(vm, arg(a, 1).r())? { c |= vm.invoke_virtual(t, "add", "(Ljava/lang/Object;)Z", &[v])?.z(); } ret_bool(c) }),
        "containsAll" "(Ljava/util/Collection;)Z" M => Some(|vm, a| { let t = this(vm, a)?; for v in iterate(vm, arg(a, 1).r())? { if !vm.invoke_virtual(t, "contains", "(Ljava/lang/Object;)Z", &[v])?.z() { return ret_bool(false); } } ret_bool(true) }),
        "stream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; super::stream::stream_of(vm, items) }),
        "parallelStream" "()Ljava/util/stream/Stream;" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; super::stream::stream_of(vm, items) }),
        "toArray" "()[Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; let refs: Vec<ObjId> = items.iter().map(|v| v.r()).collect(); ret_ref(vm.new_ref_array("Ljava/lang/Object;", &refs)?) }),
        "toArray" "([Ljava/lang/Object;)[Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; to_typed_array(vm, &items, arg(a, 1).r()) }),
        "toArray" "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; let arr = vm.invoke_virtual(arg(a, 1).r(), "apply", "(I)Ljava/lang/Object;", &[Value::Int(0)])?.r(); to_typed_array(vm, &items, arr) }),
        "removeIf" "(Ljava/util/function/Predicate;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let mut changed = false; let it = vm.invoke_virtual(t, "iterator", "()Ljava/util/Iterator;", &[])?.r(); while vm.invoke_virtual(it, "hasNext", "()Z", &[])?.z() { let v = vm.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?; if vm.invoke_virtual(arg(a, 1).r(), "test", "(Ljava/lang/Object;)Z", &[v])?.z() { vm.invoke_virtual(it, "remove", "()V", &[])?; changed = true; } } ret_bool(changed) }),
        "clear" "()V" M => Some(|vm, a| { let t = this(vm, a)?; let it = vm.invoke_virtual(t, "iterator", "()Ljava/util/Iterator;", &[])?.r(); while vm.invoke_virtual(it, "hasNext", "()Z", &[])?.z() { vm.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?; vm.invoke_virtual(it, "remove", "()V", &[])?; } void() }),
    ]),
    class("java/util/SequencedCollection", None, &["java/util/Collection"], INTERFACE, &[]),
    class("java/util/List", None, &["java/util/SequencedCollection"], INTERFACE, natives![
        "get" "(I)Ljava/lang/Object;" A => None,
        "of" "()Ljava/util/List;" S => Some(|vm, _| immutable_list(vm, Vec::new())),
        "of" "(Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| immutable_list(vm, a.to_vec())),
        "of" "([Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| { let items = vm.array_values(arg(a, 0).r()); immutable_list(vm, items) }),
        "copyOf" "(Ljava/util/Collection;)Ljava/util/List;" S => Some(|vm, a| { let items = iterate(vm, arg(a, 0).r())?; immutable_list(vm, items) }),
        "sort" "(Ljava/util/Comparator;)V" M => Some(|vm, a| { let t = this(vm, a)?; let mut items = iterate(vm, t)?; sort_values(vm, &mut items, arg(a, 1).r())?; for (i, v) in items.into_iter().enumerate() { vm.invoke_virtual(t, "set", "(ILjava/lang/Object;)Ljava/lang/Object;", &[Value::Int(i as i32), v])?; } void() }),
        "replaceAll" "(Ljava/util/function/UnaryOperator;)V" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; for (i, v) in items.into_iter().enumerate() { let n = vm.invoke_virtual(arg(a, 1).r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?; vm.invoke_virtual(t, "set", "(ILjava/lang/Object;)Ljava/lang/Object;", &[Value::Int(i as i32), n])?; } void() }),
        "getFirst" "()Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.invoke_virtual(t, "size", "()I", &[])?.i(); if n == 0 { return Err(vm.throw_new("java/util/NoSuchElementException", None)); } vm.invoke_virtual(t, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]) }),
        "getLast" "()Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.invoke_virtual(t, "size", "()I", &[])?.i(); if n == 0 { return Err(vm.throw_new("java/util/NoSuchElementException", None)); } vm.invoke_virtual(t, "get", "(I)Ljava/lang/Object;", &[Value::Int(n - 1)]) }),
    ]),
    class("java/util/Set", None, &["java/util/Collection"], INTERFACE, natives![
        "of" "()Ljava/util/Set;" S => Some(|vm, _| immutable_set(vm, Vec::new())),
        "of" "(Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| immutable_set(vm, a.to_vec())),
        "of" "([Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| { let items = vm.array_values(arg(a, 0).r()); immutable_set(vm, items) }),
        "copyOf" "(Ljava/util/Collection;)Ljava/util/Set;" S => Some(|vm, a| { let items = iterate(vm, arg(a, 0).r())?; let mut unique: Vec<Value> = Vec::new(); for v in items { let mut dup = false; for u in &unique { if equals(vm, *u, v)? { dup = true; break; } } if !dup { unique.push(v); } } immutable_set(vm, unique) }),
    ]),
    class("java/util/SequencedSet", None, &["java/util/Set", "java/util/SequencedCollection"], INTERFACE, &[]),
    class("java/util/SortedSet", None, &["java/util/Set", "java/util/SequencedSet"], INTERFACE, &[]),
    class("java/util/NavigableSet", None, &["java/util/SortedSet"], INTERFACE, &[]),
    class("java/util/Queue", None, &["java/util/Collection"], INTERFACE, &[]),
    class("java/util/Deque", None, &["java/util/Queue", "java/util/SequencedCollection"], INTERFACE, &[]),
    class("java/util/RandomAccess", None, &[], INTERFACE, &[]),
    class("java/util/Map", None, &[], INTERFACE, natives![
        "get" "(Ljava/lang/Object;)Ljava/lang/Object;" A => None,
        "of" "()Ljava/util/Map;" S => Some(|vm, _| immutable_map(vm, Vec::new())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "of" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| immutable_map(vm, a.chunks(2).map(|p| (p[0], p[1])).collect())),
        "entry" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map$Entry;" S => Some(|vm, a| { if arg(a, 0).r() == 0 || arg(a, 1).r() == 0 { return Err(vm.npe(None)); } ret_ref(new_collection(vm, "java/util/KeyValueHolder", State::Entry { key: arg(a, 0), value: arg(a, 1), map: 0 })?) }),
        "copyOf" "(Ljava/util/Map;)Ljava/util/Map;" S => Some(|vm, a| { let p = map_pairs(vm, arg(a, 0).r())?; immutable_map(vm, p) }),
        "ofEntries" "([Ljava/util/Map$Entry;)Ljava/util/Map;" S => Some(|vm, a| { let mut pairs = Vec::new(); for e in vm.ref_elements(arg(a, 0).r()) { let k = vm.invoke_virtual(e, "getKey", "()Ljava/lang/Object;", &[])?; let v = vm.invoke_virtual(e, "getValue", "()Ljava/lang/Object;", &[])?; pairs.push((k, v)); } immutable_map(vm, pairs) }),
        "getOrDefault" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let v = vm.invoke_virtual(t, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1)])?; if v.r() != 0 || vm.invoke_virtual(t, "containsKey", "(Ljava/lang/Object;)Z", &[arg(a, 1)])?.z() { Ok(v) } else { Ok(arg(a, 2)) } }),
        "forEach" "(Ljava/util/function/BiConsumer;)V" M => Some(|vm, a| { let t = this(vm, a)?; for (k, v) in map_pairs(vm, t)? { vm.invoke_virtual(arg(a, 1).r(), "accept", "(Ljava/lang/Object;Ljava/lang/Object;)V", &[k, v])?; } void() }),
        "putIfAbsent" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let v = vm.invoke_virtual(t, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1)])?; if v.r() == 0 { vm.invoke_virtual(t, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1), arg(a, 2)])?; } Ok(v) }),
        "computeIfAbsent" "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let v = vm.invoke_virtual(t, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1)])?; if v.r() != 0 { return Ok(v); } let n = vm.invoke_virtual(arg(a, 2).r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1)])?; if n.r() != 0 { vm.invoke_virtual(t, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1), n])?; } Ok(n) }),
        "merge" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; let old = vm.invoke_virtual(t, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1)])?; let n = if old.r() == 0 { arg(a, 2) } else { vm.invoke_virtual(arg(a, 3).r(), "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[old, arg(a, 2)])? }; if n.r() == 0 { vm.invoke_virtual(t, "remove", "(Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1)])?; } else { vm.invoke_virtual(t, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[arg(a, 1), n])?; } Ok(n) }),
    ]),
    class("java/util/SequencedMap", None, &["java/util/Map"], INTERFACE, &[]),
    class("java/util/SortedMap", None, &["java/util/SequencedMap"], INTERFACE, &[]),
    class("java/util/NavigableMap", None, &["java/util/SortedMap"], INTERFACE, &[]),
    class("java/util/concurrent/ConcurrentMap", None, &["java/util/Map"], INTERFACE, &[]),
    class("java/util/Map$Entry", None, &[], INTERFACE, natives![
        "getKey" "()Ljava/lang/Object;" A => None,
        "getValue" "()Ljava/lang/Object;" A => None,
        "comparingByKey" "()Ljava/util/Comparator;" S => Some(|vm, _| { let key = new_collection(vm, "java/util/function/Function$Native", State::Generic(vec![Value::Int(99)]))?; let _ = key; let f = super::invoke::getter_function(vm, "getKey")?; ret_ref(native_comparator(vm, Cmp::Comparing { key: f, cmp: 0, kind: b'L' })?) }),
        "comparingByValue" "()Ljava/util/Comparator;" S => Some(|vm, _| { let f = super::invoke::getter_function(vm, "getValue")?; ret_ref(native_comparator(vm, Cmp::Comparing { key: f, cmp: 0, kind: b'L' })?) }),
        "comparingByKey" "(Ljava/util/Comparator;)Ljava/util/Comparator;" S => Some(|vm, a| { let f = super::invoke::getter_function(vm, "getKey")?; ret_ref(native_comparator(vm, Cmp::Comparing { key: f, cmp: arg(a, 0).r(), kind: b'L' })?) }),
        "comparingByValue" "(Ljava/util/Comparator;)Ljava/util/Comparator;" S => Some(|vm, a| { let f = super::invoke::getter_function(vm, "getValue")?; ret_ref(native_comparator(vm, Cmp::Comparing { key: f, cmp: arg(a, 0).r(), kind: b'L' })?) }),
    ]),
    class("java/util/Iterator", None, &[], INTERFACE, natives![
        "hasNext" "()Z" A => None,
        "next" "()Ljava/lang/Object;" A => None,
        "remove" "()V" M => Some(|vm, _| Err(vm.throw_new("java/lang/UnsupportedOperationException", Some("remove")))),
        "forEachRemaining" "(Ljava/util/function/Consumer;)V" M => Some(|vm, a| { let t = this(vm, a)?; while vm.invoke_virtual(t, "hasNext", "()Z", &[])?.z() { let v = vm.invoke_virtual(t, "next", "()Ljava/lang/Object;", &[])?; vm.invoke_virtual(arg(a, 1).r(), "accept", "(Ljava/lang/Object;)V", &[v])?; } void() }),
    ]),
    class("java/util/ListIterator", None, &["java/util/Iterator"], INTERFACE, &[]),
    class("java/util/Enumeration", None, &[], INTERFACE, &[]),
    class("java/util/Iterator$Native", Some("java/lang/Object"), &["java/util/ListIterator", "java/util/Enumeration"], FINAL, ITER_METHODS),
    class("java/util/Iterator$Native$Extra", None, &[], 0, natives![
        "hasMoreElements" "()Z" M => Some(iter_has_next),
        "nextElement" "()Ljava/lang/Object;" M => Some(iter_next),
        "asIterator" "()Ljava/util/Iterator;" M => Some(|_, a| Ok(arg(a, 0))),
    ]),
    collection("java/util/AbstractCollection", "java/lang/Object", &["java/util/Collection"], natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; collection_text(vm, t, &items) }),
    ], || Data::None),
    collection("java/util/AbstractList", "java/util/AbstractCollection", &["java/util/List"], natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "iterator" "()Ljava/util/Iterator;" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.invoke_virtual(t, "size", "()I", &[])?.i(); let mut items = Vec::new(); for i in 0..n { items.push(vm.invoke_virtual(t, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])?); } new_iter(vm, 0, IterKind::Snapshot(items), Vec::new()) }),
        "add" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.invoke_virtual(t, "size", "()I", &[])?.i(); vm.invoke_virtual(t, "add", "(ILjava/lang/Object;)V", &[Value::Int(n), arg(a, 1)])?; ret_bool(true) }),
        "add" "(ILjava/lang/Object;)V" M => Some(|vm, _| Err(uoe(vm))),
        "set" "(ILjava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, _| Err(uoe(vm))),
        "remove" "(I)Ljava/lang/Object;" M => Some(|vm, _| Err(uoe(vm))),
        "indexOf" "(Ljava/lang/Object;)I" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; for (i, v) in items.into_iter().enumerate() { if equals(vm, arg(a, 1), v)? { return Ok(Value::Int(i as i32)); } } Ok(Value::Int(-1)) }),
        "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; let list = new_list(vm, items)?; list_op(vm, &[Value::Ref(list), arg(a, 1)], "equals") }),
        "hashCode" "()I" M => Some(|vm, a| { let t = this(vm, a)?; let items = iterate(vm, t)?; let list = new_list(vm, items)?; list_op(vm, &[Value::Ref(list)], "hashCode") }),
    ], || Data::None),
    collection("java/util/AbstractSet", "java/util/AbstractCollection", &["java/util/Set"], natives!["<init>" "()V" 0x0004 => Some(|_, _| void())], || Data::None),
    collection("java/util/AbstractQueue", "java/util/AbstractCollection", &["java/util/Queue"], natives!["<init>" "()V" 0x0004 => Some(|_, _| void())], || Data::None),
    collection("java/util/AbstractMap", "java/lang/Object", &["java/util/Map"], natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "entrySet" "()Ljava/util/Set;" A => None,
        "size" "()I" M => Some(|vm, a| { let t = this(vm, a)?; let es = vm.invoke_virtual(t, "entrySet", "()Ljava/util/Set;", &[])?.r(); vm.invoke_virtual(es, "size", "()I", &[]) }),
        "get" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| { let t = this(vm, a)?; for (k, v) in map_pairs(vm, t)? { if equals(vm, k, arg(a, 1))? { return Ok(v); } } ret_ref(0) }),
        "containsKey" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; for (k, _) in map_pairs(vm, t)? { if equals(vm, k, arg(a, 1))? { return ret_bool(true); } } ret_bool(false) }),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let pairs = map_pairs(vm, t)?; let m = new_map(vm, "java/util/LinkedHashMap")?; for (k, v) in pairs { map_put(vm, m, k, v)?; } map_op(vm, &[Value::Ref(m)], "toString") }),
    ], || Data::None),
    collection("java/util/ArrayList", "java/util/AbstractList", &["java/util/List", "java/util/RandomAccess", "java/lang/Cloneable", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/LinkedList", "java/util/AbstractList", &["java/util/List", "java/util/Deque", "java/lang/Cloneable", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/ArrayDeque", "java/util/AbstractCollection", &["java/util/Deque", "java/lang/Cloneable", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/Vector", "java/util/AbstractList", &["java/util/List", "java/util/RandomAccess", "java/lang/Cloneable", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/Stack", "java/util/Vector", &[], LIST_METHODS, list_data),
    collection("java/util/concurrent/CopyOnWriteArrayList", "java/lang/Object", &["java/util/List", "java/util/RandomAccess", "java/lang/Cloneable", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/concurrent/ConcurrentLinkedQueue", "java/util/AbstractQueue", &["java/util/Queue"], LIST_METHODS, list_data),
    collection("java/util/concurrent/ConcurrentLinkedDeque", "java/util/AbstractCollection", &["java/util/Deque"], LIST_METHODS, list_data),
    collection("java/util/concurrent/LinkedBlockingQueue", "java/util/AbstractQueue", &["java/util/concurrent/BlockingQueue", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/Arrays$ArrayList", "java/util/AbstractList", &["java/util/RandomAccess", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/ImmutableCollections$ListN", "java/util/AbstractList", &["java/util/RandomAccess", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/Collections$CopiesList", "java/util/AbstractList", &["java/util/RandomAccess", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/HashMap", "java/util/AbstractMap", &["java/util/Map", "java/lang/Cloneable", "java/io/Serializable"], HASHMAP_METHODS, map_data),
    collection("java/util/LinkedHashMap", "java/util/HashMap", &["java/util/SequencedMap"], LINKEDHASHMAP_METHODS, || { let mut m = JMap::new(); m.linked = true; Data::Jdk(Box::new(State::Map(Box::new(m)))) }),
    collection("java/util/IdentityHashMap", "java/util/AbstractMap", &["java/util/Map", "java/lang/Cloneable", "java/io/Serializable"], IDENTITYMAP_METHODS, || { let mut m = JMap::new(); m.identity = true; Data::Jdk(Box::new(State::Map(Box::new(m)))) }),
    collection("java/util/WeakHashMap", "java/util/AbstractMap", &["java/util/Map"], HASHMAP_METHODS, map_data),
    collection("java/util/Hashtable", "java/lang/Object", &["java/util/Map", "java/lang/Cloneable", "java/io/Serializable"], HASHMAP_METHODS, map_data),
    collection("java/util/concurrent/ConcurrentHashMap", "java/util/AbstractMap", &["java/util/concurrent/ConcurrentMap", "java/io/Serializable"], HASHMAP_METHODS, map_data),
    collection("java/util/ImmutableCollections$MapN", "java/util/AbstractMap", &["java/io/Serializable"], HASHMAP_METHODS, map_data),
    collection("java/util/Collections$UnmodifiableMap", "java/lang/Object", &["java/util/Map", "java/io/Serializable"], HASHMAP_METHODS, map_data),
    collection("java/util/HashSet", "java/util/AbstractSet", &["java/util/Set", "java/lang/Cloneable", "java/io/Serializable"], HASHSET_METHODS, || { let mut m = JMap::new(); m.set = true; Data::Jdk(Box::new(State::Map(Box::new(m)))) }),
    collection("java/util/LinkedHashSet", "java/util/HashSet", &["java/util/SequencedSet"], LINKEDHASHSET_METHODS, || { let mut m = JMap::new(); m.set = true; m.linked = true; Data::Jdk(Box::new(State::Map(Box::new(m)))) }),
    collection("java/util/ImmutableCollections$SetN", "java/util/AbstractSet", &["java/io/Serializable"], HASHSET_METHODS, || Data::None),
    collection("java/util/Collections$UnmodifiableSet", "java/util/AbstractSet", &["java/io/Serializable"], HASHSET_METHODS, || Data::None),
    collection("java/util/concurrent/CopyOnWriteArraySet", "java/util/AbstractSet", &["java/io/Serializable"], LINKEDHASHSET_METHODS, || { let mut m = JMap::new(); m.set = true; m.linked = true; Data::Jdk(Box::new(State::Map(Box::new(m)))) }),
    collection("java/util/TreeSet", "java/util/AbstractSet", &["java/util/NavigableSet", "java/lang/Cloneable", "java/io/Serializable"], TREESET_METHODS, || Data::Jdk(Box::new(State::Tree(Box::new(JTree { set: true, ..Default::default() }))))),
    collection("java/util/EnumMap", "java/util/AbstractMap", &["java/io/Serializable", "java/lang/Cloneable"], TREEMAP_METHODS, || Data::Jdk(Box::new(State::Tree(Box::default())))),
    class("java/util/EnumMap$Extra", None, &[], 0, natives![
        "put" "(Ljava/lang/Enum;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| map_op(vm, a, "put")),
        "<init>" "(Ljava/lang/Class;)V" M => Some(|vm, a| enum_map_init(vm, a)),
        "<init>" "(Ljava/util/EnumMap;)V" M => Some(|vm, a| enum_map_init(vm, a)),
    ]),
    collection("java/util/EnumSet", "java/util/AbstractSet", &["java/lang/Cloneable", "java/io/Serializable"], natives![
        "noneOf" "(Ljava/lang/Class;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "noneOf")),
        "allOf" "(Ljava/lang/Class;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "allOf")),
        "of" "(Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "of")),
        "of" "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "of")),
        "of" "(Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "of")),
        "of" "(Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "of")),
        "of" "(Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "of")),
        "of" "(Ljava/lang/Enum;[Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "ofArray")),
        "range" "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "range")),
        "copyOf" "(Ljava/util/Collection;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "copyOf")),
        "copyOf" "(Ljava/util/EnumSet;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "copyOf")),
        "complementOf" "(Ljava/util/EnumSet;)Ljava/util/EnumSet;" S => Some(|vm, a| enum_set(vm, a, "complementOf")),
    ], || Data::None),
    collection("java/util/RegularEnumSet", "java/util/EnumSet", &[], TREESET_METHODS, || Data::Jdk(Box::new(State::Tree(Box::new(JTree { set: true, ..Default::default() }))))),
    collection("java/util/TreeMap", "java/util/AbstractMap", &["java/util/NavigableMap", "java/lang/Cloneable", "java/io/Serializable"], TREEMAP_METHODS, || Data::Jdk(Box::new(State::Tree(Box::default())))),
    collection("java/util/concurrent/ConcurrentSkipListMap", "java/util/AbstractMap", &["java/util/NavigableMap"], TREEMAP_METHODS, || Data::Jdk(Box::new(State::Tree(Box::default())))),
    collection("java/util/PriorityQueue", "java/util/AbstractQueue", &["java/io/Serializable"], HEAP_METHODS, || Data::Jdk(Box::new(State::Heap(Box::default())))),
    collection("java/util/concurrent/PriorityBlockingQueue", "java/util/AbstractQueue", &["java/io/Serializable"], HEAP_METHODS, || Data::Jdk(Box::new(State::Heap(Box::default())))),
    collection("java/util/HashMap$KeySet", "java/util/AbstractSet", &[], VIEW_METHODS, || Data::None),
    collection("java/util/HashMap$Values", "java/util/AbstractCollection", &[], VIEW_METHODS, || Data::None),
    collection("java/util/HashMap$EntrySet", "java/util/AbstractSet", &[], VIEW_METHODS, || Data::None),
    collection("java/util/HashMap$Node", "java/lang/Object", &["java/util/Map$Entry"], ENTRY_METHODS, || Data::Jdk(Box::new(State::Entry { key: Value::NULL, value: Value::NULL, map: 0 }))),
    collection("java/util/KeyValueHolder", "java/lang/Object", &["java/util/Map$Entry"], ENTRY_METHODS, || Data::Jdk(Box::new(State::Entry { key: Value::NULL, value: Value::NULL, map: 0 }))),
    collection("java/util/AbstractMap$SimpleEntry", "java/lang/Object", &["java/util/Map$Entry", "java/io/Serializable"], ENTRY_METHODS, || Data::Jdk(Box::new(State::Entry { key: Value::NULL, value: Value::NULL, map: 0 }))),
    collection("java/util/AbstractMap$SimpleImmutableEntry", "java/lang/Object", &["java/util/Map$Entry", "java/io/Serializable"], ENTRY_METHODS, || Data::Jdk(Box::new(State::Entry { key: Value::NULL, value: Value::NULL, map: 0 }))),
    NativeClass { name: "java/util/Optional", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: OPTIONAL_METHODS, statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Optional(Value::NULL, false)))) },
    NativeClass { name: "java/util/OptionalInt", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: primitive_optional!("java/util/OptionalInt", "I", b'I', "getAsInt", "java/util/function/IntSupplier"), statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/OptionalLong", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: primitive_optional!("java/util/OptionalLong", "J", b'J', "getAsLong", "java/util/function/LongSupplier"), statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/OptionalDouble", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: primitive_optional!("java/util/OptionalDouble", "D", b'D', "getAsDouble", "java/util/function/DoubleSupplier"), statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/Random", super_name: Some("java/lang/Object"), interfaces: &["java/util/random/RandomGenerator", "java/io/Serializable"], access: PUBLIC, methods: RANDOM_METHODS, statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Random(scramble(0), None)))) },
    class("java/util/random/RandomGenerator", None, &[], INTERFACE, &[]),
    class("java/util/Objects", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![
        "equals" "(Ljava/lang/Object;Ljava/lang/Object;)Z" S => Some(|vm, a| objects_op(vm, a, "equals")),
        "deepEquals" "(Ljava/lang/Object;Ljava/lang/Object;)Z" S => Some(|vm, a| objects_op(vm, a, "deepEquals")),
        "hashCode" "(Ljava/lang/Object;)I" S => Some(|vm, a| objects_op(vm, a, "hashCode")),
        "hash" "([Ljava/lang/Object;)I" S => Some(|vm, a| objects_op(vm, a, "hash")),
        "toString" "(Ljava/lang/Object;)Ljava/lang/String;" S => Some(|vm, a| objects_op(vm, a, "toString")),
        "toString" "(Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/String;" S => Some(|vm, a| objects_op(vm, a, "toString")),
        "isNull" "(Ljava/lang/Object;)Z" S => Some(|vm, a| objects_op(vm, a, "isNull")),
        "nonNull" "(Ljava/lang/Object;)Z" S => Some(|vm, a| objects_op(vm, a, "nonNull")),
        "requireNonNull" "(Ljava/lang/Object;)Ljava/lang/Object;" S => Some(|vm, a| objects_op(vm, a, "requireNonNull")),
        "requireNonNull" "(Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/Object;" S => Some(|vm, a| objects_op(vm, a, "requireNonNull")),
        "requireNonNull" "(Ljava/lang/Object;Ljava/util/function/Supplier;)Ljava/lang/Object;" S => Some(|vm, a| objects_op(vm, a, "requireNonNull")),
        "requireNonNullElse" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" S => Some(|vm, a| objects_op(vm, a, "requireNonNullElse")),
        "requireNonNullElseGet" "(Ljava/lang/Object;Ljava/util/function/Supplier;)Ljava/lang/Object;" S => Some(|vm, a| objects_op(vm, a, "requireNonNullElseGet")),
        "compare" "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/Comparator;)I" S => Some(|vm, a| objects_op(vm, a, "compare")),
        "checkIndex" "(II)I" S => Some(|vm, a| objects_op(vm, a, "checkIndex")),
    ]),
    class("java/util/Arrays", Some("java/lang/Object"), &[], PUBLIC, ARRAYS_METHODS),
    class("java/util/Collections", Some("java/lang/Object"), &[], PUBLIC, COLLECTIONS_METHODS),
    class("java/util/Comparator", None, &[], INTERFACE, natives![
        "compare" "(Ljava/lang/Object;Ljava/lang/Object;)I" A => None,
        "reversed" "()Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "reversed")),
        "thenComparing" "(Ljava/util/Comparator;)Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "thenComparing")),
        "thenComparing" "(Ljava/util/function/Function;)Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "thenComparing")),
        "thenComparing" "(Ljava/util/function/Function;Ljava/util/Comparator;)Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "thenComparingKey")),
        "thenComparingInt" "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "thenComparingInt")),
        "thenComparingLong" "(Ljava/util/function/ToLongFunction;)Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "thenComparingLong")),
        "thenComparingDouble" "(Ljava/util/function/ToDoubleFunction;)Ljava/util/Comparator;" M => Some(|vm, a| comparator_default(vm, a, "thenComparingDouble")),
        "naturalOrder" "()Ljava/util/Comparator;" S => Some(|vm, _| ret_ref(native_comparator(vm, Cmp::Natural)?)),
        "reverseOrder" "()Ljava/util/Comparator;" S => Some(|vm, _| ret_ref(native_comparator(vm, Cmp::ReverseNatural)?)),
        "comparing" "(Ljava/util/function/Function;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Comparing { key: arg(a, 0).r(), cmp: 0, kind: b'L' })?)),
        "comparing" "(Ljava/util/function/Function;Ljava/util/Comparator;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Comparing { key: arg(a, 0).r(), cmp: arg(a, 1).r(), kind: b'L' })?)),
        "comparingInt" "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Comparing { key: arg(a, 0).r(), cmp: 0, kind: b'I' })?)),
        "comparingLong" "(Ljava/util/function/ToLongFunction;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Comparing { key: arg(a, 0).r(), cmp: 0, kind: b'J' })?)),
        "comparingDouble" "(Ljava/util/function/ToDoubleFunction;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Comparing { key: arg(a, 0).r(), cmp: 0, kind: b'D' })?)),
        "nullsFirst" "(Ljava/util/Comparator;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Nulls { first: true, cmp: arg(a, 0).r() })?)),
        "nullsLast" "(Ljava/util/Comparator;)Ljava/util/Comparator;" S => Some(|vm, a| ret_ref(native_comparator(vm, Cmp::Nulls { first: false, cmp: arg(a, 0).r() })?)),
    ]),
    collection("java/util/Comparator$Native", "java/lang/Object", &["java/util/Comparator", "java/io/Serializable"], natives!["compare" "(Ljava/lang/Object;Ljava/lang/Object;)I" M => Some(comparator_compare)], || Data::None),
    NativeClass { name: "java/util/StringJoiner", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: natives![
        "<init>" "(Ljava/lang/CharSequence;)V" M => Some(|vm, a| joiner_op(vm, a, "init")),
        "<init>" "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V" M => Some(|vm, a| joiner_op(vm, a, "init")),
        "add" "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;" M => Some(|vm, a| joiner_op(vm, a, "add")),
        "setEmptyValue" "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;" M => Some(|vm, a| joiner_op(vm, a, "setEmptyValue")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| joiner_op(vm, a, "toString")),
        "length" "()I" M => Some(|vm, a| joiner_op(vm, a, "length")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/regex/Pattern", super_name: Some("java/lang/Object"), interfaces: &["java/io/Serializable"], access: PUBLIC | FINAL, methods: natives![
        "compile" "(Ljava/lang/String;)Ljava/util/regex/Pattern;" S => Some(|vm, a| pattern_op(vm, a, "compile")),
        "compile" "(Ljava/lang/String;I)Ljava/util/regex/Pattern;" S => Some(|vm, a| pattern_op(vm, a, "compile")),
        "matches" "(Ljava/lang/String;Ljava/lang/CharSequence;)Z" S => Some(|vm, a| pattern_op(vm, a, "matches")),
        "quote" "(Ljava/lang/String;)Ljava/lang/String;" S => Some(|vm, a| pattern_op(vm, a, "quote")),
        "matcher" "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;" M => Some(|vm, a| pattern_op(vm, a, "matcher")),
        "pattern" "()Ljava/lang/String;" M => Some(|vm, a| pattern_op(vm, a, "pattern")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| pattern_op(vm, a, "toString")),
        "flags" "()I" M => Some(|vm, a| pattern_op(vm, a, "flags")),
        "split" "(Ljava/lang/CharSequence;)[Ljava/lang/String;" M => Some(|vm, a| pattern_op(vm, a, "split")),
        "split" "(Ljava/lang/CharSequence;I)[Ljava/lang/String;" M => Some(|vm, a| pattern_op(vm, a, "split")),
    ], statics: &[("CASE_INSENSITIVE", "I"), ("MULTILINE", "I"), ("DOTALL", "I"), ("LITERAL", "I"), ("COMMENTS", "I"), ("UNICODE_CASE", "I"), ("UNICODE_CHARACTER_CLASS", "I"), ("UNIX_LINES", "I")], clinit: Some(|vm, _| { let c = vm.load("java/util/regex/Pattern")?; for (n, v) in [("UNIX_LINES", 1), ("CASE_INSENSITIVE", 2), ("COMMENTS", 4), ("MULTILINE", 8), ("LITERAL", 16), ("DOTALL", 32), ("UNICODE_CASE", 64), ("UNICODE_CHARACTER_CLASS", 256)] { vm.set_static(c, n, Value::Int(v)); } void() }), data: None },
    NativeClass { name: "java/util/regex/Matcher", super_name: Some("java/lang/Object"), interfaces: &["java/util/regex/MatchResult"], access: PUBLIC | FINAL, methods: natives![
        "matches" "()Z" M => Some(|vm, a| matcher_op(vm, a, "matches")),
        "lookingAt" "()Z" M => Some(|vm, a| matcher_op(vm, a, "lookingAt")),
        "find" "()Z" M => Some(|vm, a| matcher_op(vm, a, "find")),
        "find" "(I)Z" M => Some(|vm, a| matcher_op(vm, a, "findFrom")),
        "group" "()Ljava/lang/String;" M => Some(|vm, a| matcher_op(vm, a, "group")),
        "group" "(I)Ljava/lang/String;" M => Some(|vm, a| matcher_op(vm, a, "groupN")),
        "group" "(Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| matcher_op(vm, a, "groupName")),
        "groupCount" "()I" M => Some(|vm, a| matcher_op(vm, a, "groupCount")),
        "start" "()I" M => Some(|vm, a| matcher_op(vm, a, "start")),
        "start" "(I)I" M => Some(|vm, a| matcher_op(vm, a, "startN")),
        "end" "()I" M => Some(|vm, a| matcher_op(vm, a, "end")),
        "end" "(I)I" M => Some(|vm, a| matcher_op(vm, a, "endN")),
        "reset" "()Ljava/util/regex/Matcher;" M => Some(|vm, a| matcher_op(vm, a, "reset")),
        "replaceAll" "(Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| matcher_op(vm, a, "replaceAll")),
        "replaceFirst" "(Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| matcher_op(vm, a, "replaceFirst")),
        "appendReplacement" "(Ljava/lang/StringBuilder;Ljava/lang/String;)Ljava/util/regex/Matcher;" M => Some(|vm, a| matcher_op(vm, a, "appendReplacement")),
        "appendReplacement" "(Ljava/lang/StringBuffer;Ljava/lang/String;)Ljava/util/regex/Matcher;" M => Some(|vm, a| matcher_op(vm, a, "appendReplacement")),
        "appendTail" "(Ljava/lang/StringBuilder;)Ljava/lang/StringBuilder;" M => Some(|vm, a| matcher_op(vm, a, "appendTail")),
        "appendTail" "(Ljava/lang/StringBuffer;)Ljava/lang/StringBuffer;" M => Some(|vm, a| matcher_op(vm, a, "appendTail")),
        "hitEnd" "()Z" M => Some(|vm, a| matcher_op(vm, a, "hitEnd")),
        "results" "()Ljava/util/stream/Stream;" M => Some(|vm, a| matcher_op(vm, a, "results")),
        "pattern" "()Ljava/util/regex/Pattern;" M => Some(|vm, a| matcher_op(vm, a, "pattern")),
    ], statics: &[], clinit: None, data: None },
    class("java/util/regex/MatchResult", None, &[], INTERFACE, &[]),
    atomic_class!("java/util/concurrent/atomic/AtomicInteger", "I", "init"),
    atomic_class!("java/util/concurrent/atomic/AtomicLong", "J", "initLong"),
    NativeClass { name: "java/util/concurrent/atomic/AtomicBoolean", super_name: Some("java/lang/Object"), interfaces: &["java/io/Serializable"], access: PUBLIC, methods: natives![
        "<init>" "()V" M => Some(|vm, a| atomic_op(vm, a, "init")),
        "<init>" "(Z)V" M => Some(|vm, a| atomic_op(vm, a, "init")),
        "get" "()Z" M => Some(|vm, a| atomic_op(vm, a, "get")),
        "set" "(Z)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "lazySet" "(Z)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "getAndSet" "(Z)Z" M => Some(|vm, a| atomic_op(vm, a, "getAndSet")),
        "compareAndSet" "(ZZ)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
        "weakCompareAndSet" "(ZZ)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
        "compareAndExchange" "(ZZ)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndExchange")),
        "getPlain" "()Z" M => Some(|vm, a| atomic_op(vm, a, "get")),
        "setPlain" "(Z)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| atomic_op(vm, a, "toString")),
    ], statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Atomic(Value::Int(0))))) },
    NativeClass { name: "java/util/concurrent/atomic/AtomicReference", super_name: Some("java/lang/Object"), interfaces: &["java/io/Serializable"], access: PUBLIC, methods: natives![
        "<init>" "()V" M => Some(|vm, a| atomic_op(vm, a, "initRef")),
        "<init>" "(Ljava/lang/Object;)V" M => Some(|vm, a| atomic_op(vm, a, "initRef")),
        "get" "()Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "get")),
        "getPlain" "()Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "get")),
        "set" "(Ljava/lang/Object;)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "lazySet" "(Ljava/lang/Object;)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "getAndSet" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "getAndSet")),
        "compareAndSet" "(Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
        "weakCompareAndSet" "(Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
        "weakCompareAndSetPlain" "(Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
        "weakCompareAndSetVolatile" "(Ljava/lang/Object;Ljava/lang/Object;)Z" M => Some(|vm, a| atomic_op(vm, a, "compareAndSet")),
        "compareAndExchange" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "compareAndExchange")),
        "getAcquire" "()Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "get")),
        "getOpaque" "()Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "get")),
        "setRelease" "(Ljava/lang/Object;)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "setPlain" "(Ljava/lang/Object;)V" M => Some(|vm, a| atomic_op(vm, a, "set")),
        "getAndAccumulate" "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "getAndAccumulate")),
        "updateAndGet" "(Ljava/util/function/UnaryOperator;)Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "updateAndGet")),
        "getAndUpdate" "(Ljava/util/function/UnaryOperator;)Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "getAndUpdate")),
        "accumulateAndGet" "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;" M => Some(|vm, a| atomic_op(vm, a, "accumulateAndGet")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| atomic_op(vm, a, "toString")),
    ], statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Atomic(Value::NULL)))) },
    class("java/util/TreeSet$Extra", None, &[], 0, natives![
        "<init>" "(Ljava/util/Comparator;)V" M => Some(|vm, a| tree_init(vm, a, true)),
        "<init>" "(Ljava/util/SortedSet;)V" M => Some(|vm, a| tree_init(vm, a, true)),
    ]),
    collection("java/util/concurrent/ArrayBlockingQueue", "java/util/AbstractQueue", &["java/util/concurrent/BlockingQueue", "java/io/Serializable"], LIST_METHODS, list_data),
    collection("java/util/concurrent/LinkedBlockingDeque", "java/util/AbstractQueue", &["java/util/concurrent/BlockingDeque", "java/io/Serializable"], LIST_METHODS, list_data),
    class("java/util/concurrent/atomic/AtomicInteger$Extra", None, &[], 0, natives![
        "updateAndGet" "(Ljava/util/function/IntUnaryOperator;)I" M => Some(|vm, a| atomic_op(vm, a, "updateAndGet")),
        "getAndUpdate" "(Ljava/util/function/IntUnaryOperator;)I" M => Some(|vm, a| atomic_op(vm, a, "getAndUpdate")),
        "accumulateAndGet" "(ILjava/util/function/IntBinaryOperator;)I" M => Some(|vm, a| atomic_op(vm, a, "accumulateAndGet")),
        "getAndAccumulate" "(ILjava/util/function/IntBinaryOperator;)I" M => Some(|vm, a| atomic_op(vm, a, "getAndAccumulate")),
    ]),
    class("java/util/concurrent/atomic/AtomicLong$Extra", None, &[], 0, natives![
        "updateAndGet" "(Ljava/util/function/LongUnaryOperator;)J" M => Some(|vm, a| atomic_op(vm, a, "updateAndGet")),
        "getAndUpdate" "(Ljava/util/function/LongUnaryOperator;)J" M => Some(|vm, a| atomic_op(vm, a, "getAndUpdate")),
        "accumulateAndGet" "(JLjava/util/function/LongBinaryOperator;)J" M => Some(|vm, a| atomic_op(vm, a, "accumulateAndGet")),
    ]),
    // java.util.function: the interfaces, their default and static methods,
    // and the JDK's objects for what those return.
    functional!("java/util/function/Function", "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", [
        nm("andThen", "(Ljava/util/function/Function;)Ljava/util/function/Function;", M, |vm, a| fun_object(vm, "java/util/function/Function", Fun::AndThen, &[a[0], arg(a, 1)])),
        nm("compose", "(Ljava/util/function/Function;)Ljava/util/function/Function;", M, |vm, a| fun_object(vm, "java/util/function/Function", Fun::Compose, &[a[0], arg(a, 1)])),
        nm("identity", "()Ljava/util/function/Function;", S, |vm, _| fun_object(vm, "java/util/function/Function", Fun::Identity, &[])),
    ]),
    functional_native!("java/util/function/Function$Native", "java/util/function/Function", "apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    functional!("java/util/function/BiFunction", "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", [
        nm("andThen", "(Ljava/util/function/Function;)Ljava/util/function/BiFunction;", M, |vm, a| fun_object(vm, "java/util/function/BiFunction", Fun::AndThen, &[a[0], arg(a, 1)])),
    ]),
    functional_native!("java/util/function/BiFunction$Native", "java/util/function/BiFunction", "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    functional!("java/util/function/UnaryOperator", "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", [
        nm("identity", "()Ljava/util/function/UnaryOperator;", S, |vm, _| fun_object(vm, "java/util/function/UnaryOperator", Fun::Identity, &[])),
    ]),
    functional_native!("java/util/function/UnaryOperator$Native", "java/util/function/UnaryOperator", "apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    functional!("java/util/function/BinaryOperator", "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", [
        nm("maxBy", "(Ljava/util/Comparator;)Ljava/util/function/BinaryOperator;", S, |vm, a| binary_by(vm, arg(a, 0).r(), true)),
        nm("minBy", "(Ljava/util/Comparator;)Ljava/util/function/BinaryOperator;", S, |vm, a| binary_by(vm, arg(a, 0).r(), false)),
    ]),
    collection("java/util/function/BinaryOperator$Native", "java/lang/Object", &["java/util/function/BinaryOperator"], natives!["apply" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" M => Some(binary_by_apply)], || Data::None),
    functional!("java/util/function/Consumer", "accept", "(Ljava/lang/Object;)V", [
        nm("andThen", "(Ljava/util/function/Consumer;)Ljava/util/function/Consumer;", M, |vm, a| fun_object(vm, "java/util/function/Consumer", Fun::AndThen, &[a[0], arg(a, 1)])),
    ]),
    functional_native!("java/util/function/Consumer$Native", "java/util/function/Consumer", "accept", "(Ljava/lang/Object;)V"),
    functional!("java/util/function/BiConsumer", "accept", "(Ljava/lang/Object;Ljava/lang/Object;)V", [
        nm("andThen", "(Ljava/util/function/BiConsumer;)Ljava/util/function/BiConsumer;", M, |vm, a| fun_object(vm, "java/util/function/BiConsumer", Fun::AndThen, &[a[0], arg(a, 1)])),
    ]),
    functional_native!("java/util/function/BiConsumer$Native", "java/util/function/BiConsumer", "accept", "(Ljava/lang/Object;Ljava/lang/Object;)V"),
    functional!("java/util/function/Supplier", "get", "()Ljava/lang/Object;", []),
    functional!("java/util/function/Predicate", "test", "(Ljava/lang/Object;)Z", [
        nm("negate", "()Ljava/util/function/Predicate;", M, |vm, a| fun_object(vm, "java/util/function/Predicate", Fun::Negate, &[a[0]])),
        nm("and", "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;", M, |vm, a| fun_object(vm, "java/util/function/Predicate", Fun::And, &[a[0], arg(a, 1)])),
        nm("or", "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;", M, |vm, a| fun_object(vm, "java/util/function/Predicate", Fun::Or, &[a[0], arg(a, 1)])),
        nm("isEqual", "(Ljava/lang/Object;)Ljava/util/function/Predicate;", S, |vm, a| fun_object(vm, "java/util/function/Predicate", Fun::IsEqual, &[arg(a, 0)])),
        nm("not", "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;", S, |vm, a| fun_object(vm, "java/util/function/Predicate", Fun::Negate, &[arg(a, 0)])),
    ]),
    functional_native!("java/util/function/Predicate$Native", "java/util/function/Predicate", "test", "(Ljava/lang/Object;)Z"),
    functional!("java/util/function/BiPredicate", "test", "(Ljava/lang/Object;Ljava/lang/Object;)Z", [
        nm("negate", "()Ljava/util/function/BiPredicate;", M, |vm, a| fun_object(vm, "java/util/function/BiPredicate", Fun::Negate, &[a[0]])),
        nm("and", "(Ljava/util/function/BiPredicate;)Ljava/util/function/BiPredicate;", M, |vm, a| fun_object(vm, "java/util/function/BiPredicate", Fun::And, &[a[0], arg(a, 1)])),
        nm("or", "(Ljava/util/function/BiPredicate;)Ljava/util/function/BiPredicate;", M, |vm, a| fun_object(vm, "java/util/function/BiPredicate", Fun::Or, &[a[0], arg(a, 1)])),
    ]),
    functional_native!("java/util/function/BiPredicate$Native", "java/util/function/BiPredicate", "test", "(Ljava/lang/Object;Ljava/lang/Object;)Z"),
    functional!("java/util/function/IntFunction", "apply", "(I)Ljava/lang/Object;", []),
    functional!("java/util/function/LongFunction", "apply", "(J)Ljava/lang/Object;", []),
    functional!("java/util/function/DoubleFunction", "apply", "(D)Ljava/lang/Object;", []),
    functional!("java/util/function/ToIntFunction", "applyAsInt", "(Ljava/lang/Object;)I", []),
    functional!("java/util/function/ToLongFunction", "applyAsLong", "(Ljava/lang/Object;)J", []),
    functional!("java/util/function/ToDoubleFunction", "applyAsDouble", "(Ljava/lang/Object;)D", []),
    functional!("java/util/function/ToIntBiFunction", "applyAsInt", "(Ljava/lang/Object;Ljava/lang/Object;)I", []),
    functional!("java/util/function/IntUnaryOperator", "applyAsInt", "(I)I", [
        nm("andThen", "(Ljava/util/function/IntUnaryOperator;)Ljava/util/function/IntUnaryOperator;", M, |vm, a| fun_object(vm, "java/util/function/IntUnaryOperator", Fun::AndThen, &[a[0], arg(a, 1)])),
        nm("compose", "(Ljava/util/function/IntUnaryOperator;)Ljava/util/function/IntUnaryOperator;", M, |vm, a| fun_object(vm, "java/util/function/IntUnaryOperator", Fun::Compose, &[a[0], arg(a, 1)])),
        nm("identity", "()Ljava/util/function/IntUnaryOperator;", S, |vm, _| fun_object(vm, "java/util/function/IntUnaryOperator", Fun::Identity, &[])),
    ]),
    functional_native!("java/util/function/IntUnaryOperator$Native", "java/util/function/IntUnaryOperator", "applyAsInt", "(I)I"),
    functional!("java/util/function/LongUnaryOperator", "applyAsLong", "(J)J", []),
    functional!("java/util/function/DoubleUnaryOperator", "applyAsDouble", "(D)D", []),
    functional!("java/util/function/IntBinaryOperator", "applyAsInt", "(II)I", []),
    functional!("java/util/function/LongBinaryOperator", "applyAsLong", "(JJ)J", []),
    functional!("java/util/function/DoubleBinaryOperator", "applyAsDouble", "(DD)D", []),
    functional!("java/util/function/IntPredicate", "test", "(I)Z", [
        nm("negate", "()Ljava/util/function/IntPredicate;", M, |vm, a| fun_object(vm, "java/util/function/IntPredicate", Fun::Negate, &[a[0]])),
    ]),
    functional_native!("java/util/function/IntPredicate$Native", "java/util/function/IntPredicate", "test", "(I)Z"),
    functional!("java/util/function/LongPredicate", "test", "(J)Z", []),
    functional!("java/util/function/DoublePredicate", "test", "(D)Z", []),
    functional!("java/util/function/IntConsumer", "accept", "(I)V", []),
    functional!("java/util/function/LongConsumer", "accept", "(J)V", []),
    functional!("java/util/function/DoubleConsumer", "accept", "(D)V", []),
    functional!("java/util/function/ObjIntConsumer", "accept", "(Ljava/lang/Object;I)V", []),
    functional!("java/util/function/IntSupplier", "getAsInt", "()I", []),
    functional!("java/util/function/LongSupplier", "getAsLong", "()J", []),
    functional!("java/util/function/DoubleSupplier", "getAsDouble", "()D", []),
    functional!("java/util/function/BooleanSupplier", "getAsBoolean", "()Z", []),
    functional!("java/util/function/IntToLongFunction", "applyAsLong", "(I)J", []),
    functional!("java/util/function/IntToDoubleFunction", "applyAsDouble", "(I)D", []),
    functional!("java/util/function/DoubleToIntFunction", "applyAsInt", "(D)I", []),
    functional!("java/util/function/LongToIntFunction", "applyAsInt", "(J)I", []),
];

fn binary_by(vm: &mut Vm, cmp: ObjId, max: bool) -> Res<Value> {
    ret_ref(new_collection(vm, "java/util/function/BinaryOperator$Native", State::Generic(vec![Value::Ref(cmp), Value::Int(max as i32)]))?)
}

fn binary_by_apply(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Generic(v)) = state(vm, t).cloned() else { return void() };
    let c = compare(vm, arg(a, 1), arg(a, 2), v[0].r())?;
    Ok(if v[1].i() != 0 { if c >= 0 { arg(a, 1) } else { arg(a, 2) } } else if c <= 0 { arg(a, 1) } else { arg(a, 2) })
}

static ARRAYS_METHODS: &[NativeMethod] = natives![
    "toString" "([I)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([J)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([S)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([B)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([C)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([Z)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([F)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([D)Ljava/lang/String;" S => Some(arrays_to_string),
    "toString" "([Ljava/lang/Object;)Ljava/lang/String;" S => Some(arrays_to_string),
    "deepToString" "([Ljava/lang/Object;)Ljava/lang/String;" S => Some(|vm, a| { let mut out = Vec::new(); deep_to_string(vm, arg(a, 0).r(), &mut out, &mut Vec::new())?; ret_ref(vm.new_string_units(out)?) }),
    "sort" "([I)V" S => Some(arrays_sort),
    "sort" "([J)V" S => Some(arrays_sort),
    "sort" "([S)V" S => Some(arrays_sort),
    "sort" "([B)V" S => Some(arrays_sort),
    "sort" "([C)V" S => Some(arrays_sort),
    "sort" "([F)V" S => Some(arrays_sort),
    "sort" "([D)V" S => Some(arrays_sort),
    "sort" "([III)V" S => Some(arrays_sort),
    "sort" "([JII)V" S => Some(arrays_sort),
    "sort" "([DII)V" S => Some(arrays_sort),
    "sort" "([Ljava/lang/Object;)V" S => Some(arrays_sort),
    "sort" "([Ljava/lang/Object;II)V" S => Some(arrays_sort),
    "sort" "([Ljava/lang/Object;Ljava/util/Comparator;)V" S => Some(arrays_sort),
    "sort" "([Ljava/lang/Object;IILjava/util/Comparator;)V" S => Some(arrays_sort),
    "parallelSort" "([I)V" S => Some(arrays_sort),
    "fill" "([IJ)V" S => Some(arrays_fill),
    "fill" "([II)V" S => Some(arrays_fill),
    "fill" "([JJ)V" S => Some(arrays_fill),
    "fill" "([SS)V" S => Some(arrays_fill),
    "fill" "([BB)V" S => Some(arrays_fill),
    "fill" "([CC)V" S => Some(arrays_fill),
    "fill" "([ZZ)V" S => Some(arrays_fill),
    "fill" "([FF)V" S => Some(arrays_fill),
    "fill" "([DD)V" S => Some(arrays_fill),
    "fill" "([Ljava/lang/Object;Ljava/lang/Object;)V" S => Some(arrays_fill),
    "fill" "([IIII)V" S => Some(arrays_fill),
    "fill" "([JIIJ)V" S => Some(arrays_fill),
    "fill" "([CIIC)V" S => Some(arrays_fill),
    "fill" "([BIIB)V" S => Some(arrays_fill),
    "fill" "([ZIIZ)V" S => Some(arrays_fill),
    "fill" "([DIID)V" S => Some(arrays_fill),
    "fill" "([Ljava/lang/Object;IILjava/lang/Object;)V" S => Some(arrays_fill),
    "copyOf" "([II)[I" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([JI)[J" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([SI)[S" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([BI)[B" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([CI)[C" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([ZI)[Z" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([FI)[F" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([DI)[D" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([Ljava/lang/Object;I)[Ljava/lang/Object;" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOf" "([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;" S => Some(|vm, a| copy_of(vm, a, false)),
    "copyOfRange" "([III)[I" S => Some(|vm, a| copy_of(vm, a, true)),
    "copyOfRange" "([JII)[J" S => Some(|vm, a| copy_of(vm, a, true)),
    "copyOfRange" "([BII)[B" S => Some(|vm, a| copy_of(vm, a, true)),
    "copyOfRange" "([CII)[C" S => Some(|vm, a| copy_of(vm, a, true)),
    "copyOfRange" "([DII)[D" S => Some(|vm, a| copy_of(vm, a, true)),
    "copyOfRange" "([Ljava/lang/Object;II)[Ljava/lang/Object;" S => Some(|vm, a| copy_of(vm, a, true)),
    "equals" "([I[I)Z" S => Some(arrays_equals),
    "equals" "([J[J)Z" S => Some(arrays_equals),
    "equals" "([S[S)Z" S => Some(arrays_equals),
    "equals" "([B[B)Z" S => Some(arrays_equals),
    "equals" "([C[C)Z" S => Some(arrays_equals),
    "equals" "([Z[Z)Z" S => Some(arrays_equals),
    "equals" "([F[F)Z" S => Some(arrays_equals),
    "equals" "([D[D)Z" S => Some(arrays_equals),
    "equals" "([Ljava/lang/Object;[Ljava/lang/Object;)Z" S => Some(arrays_equals),
    "deepEquals" "([Ljava/lang/Object;[Ljava/lang/Object;)Z" S => Some(|vm, a| ret_bool(deep_equals(vm, arg(a, 0).r(), arg(a, 1).r())?)),
    "hashCode" "([I)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?))),
    "hashCode" "([J)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?))),
    "hashCode" "([B)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?))),
    "hashCode" "([C)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?))),
    "hashCode" "([D)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?))),
    "hashCode" "([Ljava/lang/Object;)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), false)?))),
    "deepHashCode" "([Ljava/lang/Object;)I" S => Some(|vm, a| Ok(Value::Int(array_hash(vm, arg(a, 0).r(), true)?))),
    "binarySearch" "([II)I" S => Some(binary_search),
    "binarySearch" "([JJ)I" S => Some(binary_search),
    "binarySearch" "([DD)I" S => Some(binary_search),
    "binarySearch" "([Ljava/lang/Object;Ljava/lang/Object;)I" S => Some(binary_search),
    "binarySearch" "([Ljava/lang/Object;Ljava/lang/Object;Ljava/util/Comparator;)I" S => Some(binary_search),
    "binarySearch" "([IIII)I" S => Some(binary_search),
    "asList" "([Ljava/lang/Object;)Ljava/util/List;" S => Some(as_list),
    "stream" "([Ljava/lang/Object;)Ljava/util/stream/Stream;" S => Some(|vm, a| { let v = vm.array_values(arg(a, 0).r()); super::stream::stream_of(vm, v) }),
    "stream" "([I)Ljava/util/stream/IntStream;" S => Some(|vm, a| { let v = vm.array_values(arg(a, 0).r()); super::stream::int_stream_of(vm, v) }),
    "stream" "([J)Ljava/util/stream/LongStream;" S => Some(|vm, a| { let v = vm.array_values(arg(a, 0).r()); super::stream::long_stream_of(vm, v) }),
    "stream" "([D)Ljava/util/stream/DoubleStream;" S => Some(|vm, a| { let v = vm.array_values(arg(a, 0).r()); super::stream::double_stream_of(vm, v) }),
    "stream" "([Ljava/lang/Object;II)Ljava/util/stream/Stream;" S => Some(|vm, a| { let v = vm.array_values(arg(a, 0).r()); let (s, e) = (arg(a, 1).i() as usize, arg(a, 2).i() as usize); super::stream::stream_of(vm, v[s.min(v.len())..e.min(v.len())].to_vec()) }),
    "compare" "([Z[Z)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([B[B)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([S[S)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([C[C)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([I[I)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([J[J)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([F[F)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([D[D)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([Ljava/lang/Comparable;[Ljava/lang/Comparable;)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compare" "([Ljava/lang/Object;[Ljava/lang/Object;Ljava/util/Comparator;)I" S => Some(|vm, a| arrays_compare(vm, a, false)),
    "compareUnsigned" "([B[B)I" S => Some(|vm, a| arrays_compare(vm, a, true)),
    "compareUnsigned" "([S[S)I" S => Some(|vm, a| arrays_compare(vm, a, true)),
    "compareUnsigned" "([I[I)I" S => Some(|vm, a| arrays_compare(vm, a, true)),
    "compareUnsigned" "([J[J)I" S => Some(|vm, a| arrays_compare(vm, a, true)),
    "mismatch" "([Z[Z)I" S => Some(arrays_mismatch),
    "mismatch" "([B[B)I" S => Some(arrays_mismatch),
    "mismatch" "([C[C)I" S => Some(arrays_mismatch),
    "mismatch" "([S[S)I" S => Some(arrays_mismatch),
    "mismatch" "([I[I)I" S => Some(arrays_mismatch),
    "mismatch" "([J[J)I" S => Some(arrays_mismatch),
    "mismatch" "([F[F)I" S => Some(arrays_mismatch),
    "mismatch" "([D[D)I" S => Some(arrays_mismatch),
    "mismatch" "([Ljava/lang/Object;[Ljava/lang/Object;)I" S => Some(arrays_mismatch),
    "setAll" "([Ljava/lang/Object;Ljava/util/function/IntFunction;)V" S => Some(|vm, a| { let arr = arg(a, 0).r(); let n = vm.array(arr).map_or(0, Array::len); for i in 0..n { let v = vm.invoke_virtual(arg(a, 1).r(), "apply", "(I)Ljava/lang/Object;", &[Value::Int(i as i32)])?; vm.ref_array_set(arr, i, v.r()); } void() }),
    "setAll" "([ILjava/util/function/IntUnaryOperator;)V" S => Some(|vm, a| { let arr = arg(a, 0).r(); let n = vm.array(arr).map_or(0, Array::len); for i in 0..n { let v = vm.invoke_virtual(arg(a, 1).r(), "applyAsInt", "(I)I", &[Value::Int(i as i32)])?; vm.array_mut(arr).unwrap().set(i, v); } void() }),
];

static COLLECTIONS_METHODS: &[NativeMethod] = natives![
    "sort" "(Ljava/util/List;)V" S => Some(|vm, a| collections_op(vm, a, "sort")),
    "sort" "(Ljava/util/List;Ljava/util/Comparator;)V" S => Some(|vm, a| collections_op(vm, a, "sort")),
    "reverse" "(Ljava/util/List;)V" S => Some(|vm, a| collections_op(vm, a, "reverse")),
    "shuffle" "(Ljava/util/List;)V" S => Some(|vm, a| collections_op(vm, a, "shuffle")),
    "shuffle" "(Ljava/util/List;Ljava/util/Random;)V" S => Some(|vm, a| collections_op(vm, a, "shuffle")),
    "swap" "(Ljava/util/List;II)V" S => Some(|vm, a| collections_op(vm, a, "swap")),
    "rotate" "(Ljava/util/List;I)V" S => Some(|vm, a| collections_op(vm, a, "rotate")),
    "max" "(Ljava/util/Collection;)Ljava/lang/Object;" S => Some(|vm, a| collections_op(vm, a, "max")),
    "max" "(Ljava/util/Collection;Ljava/util/Comparator;)Ljava/lang/Object;" S => Some(|vm, a| collections_op(vm, a, "max")),
    "min" "(Ljava/util/Collection;)Ljava/lang/Object;" S => Some(|vm, a| collections_op(vm, a, "min")),
    "min" "(Ljava/util/Collection;Ljava/util/Comparator;)Ljava/lang/Object;" S => Some(|vm, a| collections_op(vm, a, "min")),
    "frequency" "(Ljava/util/Collection;Ljava/lang/Object;)I" S => Some(|vm, a| collections_op(vm, a, "frequency")),
    "nCopies" "(ILjava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| collections_op(vm, a, "nCopies")),
    "addAll" "(Ljava/util/Collection;[Ljava/lang/Object;)Z" S => Some(|vm, a| collections_op(vm, a, "addAll")),
    "disjoint" "(Ljava/util/Collection;Ljava/util/Collection;)Z" S => Some(|vm, a| collections_op(vm, a, "disjoint")),
    "binarySearch" "(Ljava/util/List;Ljava/lang/Object;)I" S => Some(|vm, a| collections_op(vm, a, "binarySearch")),
    "binarySearch" "(Ljava/util/List;Ljava/lang/Object;Ljava/util/Comparator;)I" S => Some(|vm, a| collections_op(vm, a, "binarySearch")),
    "emptyList" "()Ljava/util/List;" S => Some(|vm, _| immutable_list(vm, Vec::new())),
    "emptySet" "()Ljava/util/Set;" S => Some(|vm, _| immutable_set(vm, Vec::new())),
    "emptyMap" "()Ljava/util/Map;" S => Some(|vm, _| immutable_map(vm, Vec::new())),
    "enumeration" "(Ljava/util/Collection;)Ljava/util/Enumeration;" S => Some(|vm, a| { let items = iterate(vm, arg(a, 0).r())?; new_iter(vm, 0, IterKind::Snapshot(items), Vec::new()) }),
    "emptyEnumeration" "()Ljava/util/Enumeration;" S => Some(|vm, _| new_iter(vm, 0, IterKind::Snapshot(Vec::new()), Vec::new())),
    "emptyIterator" "()Ljava/util/Iterator;" S => Some(|vm, _| new_iter(vm, 0, IterKind::Snapshot(Vec::new()), Vec::new())),
    "singletonList" "(Ljava/lang/Object;)Ljava/util/List;" S => Some(|vm, a| ret_ref(new_collection(vm, "java/util/ImmutableCollections$ListN", State::List(JList { items: vec![arg(a, 0)], immutable: true, ..Default::default() }))?)),
    "singleton" "(Ljava/lang/Object;)Ljava/util/Set;" S => Some(|vm, a| { let l = new_list(vm, vec![arg(a, 0)])?; unmodifiable(vm, l, "set") }),
    "singletonMap" "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;" S => Some(|vm, a| { let m = new_map(vm, "java/util/LinkedHashMap")?; map_put(vm, m, arg(a, 0), arg(a, 1))?; unmodifiable(vm, m, "map") }),
    "unmodifiableList" "(Ljava/util/List;)Ljava/util/List;" S => Some(|vm, a| unmodifiable(vm, arg(a, 0).r(), "list")),
    "unmodifiableCollection" "(Ljava/util/Collection;)Ljava/util/Collection;" S => Some(|vm, a| unmodifiable(vm, arg(a, 0).r(), "list")),
    "unmodifiableSet" "(Ljava/util/Set;)Ljava/util/Set;" S => Some(|vm, a| unmodifiable(vm, arg(a, 0).r(), "set")),
    "unmodifiableMap" "(Ljava/util/Map;)Ljava/util/Map;" S => Some(|vm, a| unmodifiable(vm, arg(a, 0).r(), "map")),
    "synchronizedList" "(Ljava/util/List;)Ljava/util/List;" S => Some(|_, a| Ok(arg(a, 0))),
    "synchronizedMap" "(Ljava/util/Map;)Ljava/util/Map;" S => Some(|_, a| Ok(arg(a, 0))),
    "synchronizedSet" "(Ljava/util/Set;)Ljava/util/Set;" S => Some(|_, a| Ok(arg(a, 0))),
    "reverseOrder" "()Ljava/util/Comparator;" S => Some(|vm, _| ret_ref(native_comparator(vm, Cmp::ReverseNatural)?)),
    "reverseOrder" "(Ljava/util/Comparator;)Ljava/util/Comparator;" S => Some(|vm, a| { let c = arg(a, 0).r(); ret_ref(native_comparator(vm, if c == 0 { Cmp::ReverseNatural } else { Cmp::Reverse(c) })?) }),
    "list" "(Ljava/util/Enumeration;)Ljava/util/ArrayList;" S => Some(|_, _| Err(Exc::Unsupported("Collections.list".into()))),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn order_of(keys: &[i32]) -> Vec<i32> {
        let mut m = JMap::new();
        for &k in keys {
            // Integer.hashCode is the value.
            m.insert(JMap::spread(k), Value::Int(k), Value::Int(0));
        }
        m.order().iter().map(|&s| m.entry(s).unwrap().key.i()).collect()
    }

    #[test]
    fn hash_map_iterates_in_hotspot_order() {
        // HotSpot 25: new HashMap<Integer, _>() filled with these keys prints
        // its keySet in this order.
        assert_eq!(order_of(&[100, 3, 17, 33, 64, 1]), vec![64, 17, 33, 1, 3, 100]);
        let keys: Vec<i32> = (0..40).map(|i| i * 37 - 500).collect();
        let order = order_of(&keys);
        assert_eq!(&order[..6], &[832, -130, 129, -389, 388, 647]);
    }
}

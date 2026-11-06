//! `java.util.stream`: lazy pipelines that pull one element at a time through
//! their operations, so side effects in `peek`, `map` or `filter` happen in
//! the order Java's do; `sorted` is the barrier it is in Java, `limit` stops
//! pulling. Collectors accumulate element by element as Java's do.
use super::util::{self, compare, equals, iterate, map_get, map_put, new_list, new_optional, sort_values};
use super::{A, FINAL, INTERFACE, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Array, Data, Exc, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

#[derive(Debug, Clone)]
pub enum Src {
    Items(Vec<Value>, usize),
    Iterate { cur: Value, next: ObjId, has_next: ObjId, started: bool, done: bool },
    Generate(ObjId),
    /// `[from, to)` of an int or long range.
    Range(i64, i64),
    /// A `java.util.Iterator`.
    Iter(ObjId),
    Concat(Box<JStream>, Box<JStream>, bool),
}

#[derive(Debug, Clone)]
pub enum Op {
    Map { f: ObjId, name: &'static str, desc: &'static str, out: u8 },
    Filter { f: ObjId, desc: &'static str },
    Peek { f: ObjId, desc: &'static str },
    FlatMap { f: ObjId, desc: &'static str, inner: Option<Box<JStream>>, out: u8 },
    Limit(i64),
    Skip(i64),
    Distinct(Vec<Value>),
    Sorted { cmp: ObjId, buf: Option<Vec<Value>>, pos: usize },
    TakeWhile { f: ObjId, desc: &'static str, done: bool },
    DropWhile { f: ObjId, desc: &'static str, dropping: bool },
    /// A primitive boxed (`boxed()`), by kind.
    Box(u8),
    /// A primitive widened (`asLongStream`, `asDoubleStream`).
    Widen(u8),
}

#[derive(Debug, Clone)]
pub struct JStream {
    pub src: Src,
    pub ops: Vec<Op>,
    /// The kind of element the source makes: `L`, `I`, `J` or `D`.
    pub src_kind: u8,
    /// The kind of element the pipeline ends with.
    pub kind: u8,
    pub consumed: bool,
    pub close_handlers: Vec<ObjId>,
    /// An iterator's element pulled ahead by `hasNext`.
    pub peeked: Option<Option<Value>>,
}

impl JStream {
    fn new(src: Src, kind: u8) -> JStream {
        JStream { src, ops: Vec::new(), src_kind: kind, kind, consumed: false, close_handlers: Vec::new(), peeked: None }
    }

    pub fn refs(&self, f: &mut dyn FnMut(Value)) {
        match &self.src {
            Src::Items(v, _) => v.iter().for_each(|x| f(*x)),
            Src::Iterate { cur, next, has_next, .. } => {
                f(*cur);
                f(Value::Ref(*next));
                f(Value::Ref(*has_next));
            }
            Src::Generate(g) | Src::Iter(g) => f(Value::Ref(*g)),
            Src::Range(..) => {}
            Src::Concat(a, b, _) => {
                a.refs(f);
                b.refs(f);
            }
        }
        for op in &self.ops {
            match op {
                Op::Map { f: x, .. } | Op::Filter { f: x, .. } | Op::Peek { f: x, .. } | Op::TakeWhile { f: x, .. } | Op::DropWhile { f: x, .. } => f(Value::Ref(*x)),
                Op::FlatMap { f: x, inner, .. } => {
                    f(Value::Ref(*x));
                    if let Some(i) = inner {
                        i.refs(f);
                    }
                }
                Op::Distinct(seen) => seen.iter().for_each(|v| f(*v)),
                Op::Sorted { cmp, buf, .. } => {
                    f(Value::Ref(*cmp));
                    if let Some(b) = buf {
                        b.iter().for_each(|v| f(*v));
                    }
                }
                _ => {}
            }
        }
        self.close_handlers.iter().for_each(|h| f(Value::Ref(*h)));
    }

    /// The element count, when the pipeline knows it without running (Java's
    /// `count()` then runs nothing).
    fn known_size(&self) -> Option<i64> {
        let mut n = match &self.src {
            Src::Items(v, pos) => (v.len() - pos) as i64,
            Src::Range(a, b) => (b - a).max(0),
            _ => return None,
        };
        for op in &self.ops {
            match op {
                Op::Map { .. } | Op::Peek { .. } | Op::Sorted { .. } | Op::Box(_) | Op::Widen(_) => {}
                Op::Limit(l) => n = n.min(*l),
                Op::Skip(s) => n = (n - s).max(0),
                _ => return None,
            }
        }
        Some(n)
    }
}

fn class_of_kind(kind: u8) -> &'static str {
    match kind {
        b'I' => "java/util/stream/IntPipeline",
        b'J' => "java/util/stream/LongPipeline",
        b'D' => "java/util/stream/DoublePipeline",
        _ => "java/util/stream/ReferencePipeline",
    }
}

fn new_stream(vm: &mut Vm, s: JStream) -> Res<Value> {
    let c = vm.load(class_of_kind(s.kind))?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Stream(Box::new(s))));
    }
    ret_ref(r)
}

pub fn stream_of(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    new_stream(vm, JStream::new(Src::Items(items, 0), b'L'))
}

pub fn int_stream_of(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    new_stream(vm, JStream::new(Src::Items(items, 0), b'I'))
}

pub fn long_stream_of(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    new_stream(vm, JStream::new(Src::Items(items, 0), b'J'))
}

pub fn double_stream_of(vm: &mut Vm, items: Vec<Value>) -> Res<Value> {
    new_stream(vm, JStream::new(Src::Items(items, 0), b'D'))
}

/// Takes a stream's pipeline out of its object, which can then not be used again.
fn take(vm: &mut Vm, r: ObjId) -> Res<JStream> {
    let ok = matches!(state(vm, r), Some(State::Stream(s)) if !s.consumed);
    if !ok {
        if matches!(state(vm, r), Some(State::Stream(_))) {
            return Err(vm.throw_new("java/lang/IllegalStateException", Some("stream has already been operated upon or closed")));
        }
        return Err(Exc::Internal("not a stream".into()));
    }
    let Some(State::Stream(s)) = super::state_mut(vm, r) else { unreachable!() };
    let out = (**s).clone();
    s.consumed = true;
    s.src = Src::Items(Vec::new(), 0);
    s.ops.clear();
    Ok(out)
}

/// Descriptors of the functional interfaces a stream of `kind` calls.
fn consumer(kind: u8) -> &'static str {
    match kind {
        b'I' => "(I)V",
        b'J' => "(J)V",
        b'D' => "(D)V",
        _ => "(Ljava/lang/Object;)V",
    }
}

fn predicate(kind: u8) -> &'static str {
    match kind {
        b'I' => "(I)Z",
        b'J' => "(J)Z",
        b'D' => "(D)Z",
        _ => "(Ljava/lang/Object;)Z",
    }
}

fn unary(kind: u8) -> (&'static str, &'static str) {
    match kind {
        b'I' => ("applyAsInt", "(I)I"),
        b'J' => ("applyAsLong", "(J)J"),
        b'D' => ("applyAsDouble", "(D)D"),
        _ => ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    }
}

fn binary(kind: u8) -> (&'static str, &'static str) {
    match kind {
        b'I' => ("applyAsInt", "(II)I"),
        b'J' => ("applyAsLong", "(JJ)J"),
        b'D' => ("applyAsDouble", "(DD)D"),
        _ => ("apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    }
}

fn to_obj(kind: u8) -> &'static str {
    match kind {
        b'I' => "(I)Ljava/lang/Object;",
        b'J' => "(J)Ljava/lang/Object;",
        b'D' => "(D)Ljava/lang/Object;",
        _ => "(Ljava/lang/Object;)Ljava/lang/Object;",
    }
}

/// The function a `mapToX` of a `kind` stream calls to make a `to` element.
fn convert(kind: u8, to: u8) -> (&'static str, &'static str) {
    match (kind, to) {
        (b'L', b'I') => ("applyAsInt", "(Ljava/lang/Object;)I"),
        (b'L', b'J') => ("applyAsLong", "(Ljava/lang/Object;)J"),
        (b'L', b'D') => ("applyAsDouble", "(Ljava/lang/Object;)D"),
        (b'I', b'J') => ("applyAsLong", "(I)J"),
        (b'I', b'D') => ("applyAsDouble", "(I)D"),
        (b'J', b'I') => ("applyAsInt", "(J)I"),
        (b'J', b'D') => ("applyAsDouble", "(J)D"),
        (b'D', b'I') => ("applyAsInt", "(D)I"),
        (b'D', b'J') => ("applyAsLong", "(D)J"),
        (k, b'L') => ("apply", to_obj(k)),
        (k, _) => unary(k),
    }
}

fn pull_src(vm: &mut Vm, src: &mut Src, kind: u8) -> Res<Option<Value>> {
    match src {
        Src::Items(v, pos) => {
            let item = v.get(*pos).copied();
            if item.is_some() {
                *pos += 1;
            }
            Ok(item)
        }
        Src::Range(a, b) => {
            if *a >= *b {
                return Ok(None);
            }
            let v = *a;
            *a += 1;
            Ok(Some(if kind == b'J' { Value::Long(v) } else { Value::Int(v as i32) }))
        }
        Src::Generate(g) => {
            let (name, desc) = match kind {
                b'I' => ("getAsInt", "()I"),
                b'J' => ("getAsLong", "()J"),
                b'D' => ("getAsDouble", "()D"),
                _ => ("get", "()Ljava/lang/Object;"),
            };
            Ok(Some(vm.invoke_virtual(*g, name, desc, &[])?))
        }
        Src::Iterate { cur, next, has_next, started, done } => {
            if *done {
                return Ok(None);
            }
            if *started {
                let (name, desc) = unary(kind);
                *cur = vm.invoke_virtual(*next, name, desc, &[*cur])?;
            }
            *started = true;
            if *has_next != 0 && !vm.invoke_virtual(*has_next, "test", predicate(kind), &[*cur])?.z() {
                *done = true;
                return Ok(None);
            }
            Ok(Some(*cur))
        }
        Src::Iter(it) => {
            if !vm.invoke_virtual(*it, "hasNext", "()Z", &[])?.z() {
                return Ok(None);
            }
            Ok(Some(vm.invoke_virtual(*it, "next", "()Ljava/lang/Object;", &[])?))
        }
        Src::Concat(a, b, second) => {
            if !*second {
                if let Some(v) = pull(vm, a)? {
                    return Ok(Some(v));
                }
                *second = true;
            }
            pull(vm, b)
        }
    }
}

/// The pipeline's next element.
fn pull(vm: &mut Vm, s: &mut JStream) -> Res<Option<Value>> {
    let k = s.ops.len();
    pull_at(vm, s, k)
}

/// The next element after the first `k` operations.
fn pull_at(vm: &mut Vm, s: &mut JStream, k: usize) -> Res<Option<Value>> {
    if k == 0 {
        return pull_src(vm, &mut s.src, s.src_kind);
    }
    let in_kind = kind_before(s, k - 1);
    loop {
        // Operations that serve from what they hold first.
        match &mut s.ops[k - 1] {
            Op::Limit(n) => {
                if *n <= 0 {
                    return Ok(None);
                }
                *n -= 1;
                return pull_at(vm, s, k - 1);
            }
            Op::Sorted { cmp, buf, pos } => {
                if buf.is_none() {
                    let cmp = *cmp;
                    let mut all = Vec::new();
                    while let Some(v) = pull_at(vm, s, k - 1)? {
                        all.push(v);
                    }
                    sort_kind(vm, &mut all, in_kind, cmp)?;
                    if let Op::Sorted { buf, .. } = &mut s.ops[k - 1] {
                        *buf = Some(all);
                    }
                    continue;
                }
                let v = buf.as_ref().and_then(|b| b.get(*pos).copied());
                *pos += 1;
                return Ok(v);
            }
            Op::FlatMap { inner: Some(inner), .. } => {
                if let Some(v) = pull(vm, inner)? {
                    return Ok(Some(v));
                }
                if let Op::FlatMap { inner, .. } = &mut s.ops[k - 1] {
                    *inner = None;
                }
            }
            Op::TakeWhile { done: true, .. } => return Ok(None),
            _ => {}
        }
        let Some(v) = pull_at(vm, s, k - 1)? else { return Ok(None) };
        match s.ops[k - 1].clone() {
            Op::Map { f, name, desc, .. } => return Ok(Some(vm.invoke_virtual(f, name, desc, &[v])?)),
            Op::Filter { f, desc } => {
                if vm.invoke_virtual(f, "test", desc, &[v])?.z() {
                    return Ok(Some(v));
                }
            }
            Op::Peek { f, desc } => {
                vm.invoke_virtual(f, "accept", desc, &[v])?;
                return Ok(Some(v));
            }
            Op::FlatMap { f, desc, .. } => {
                let r = vm.invoke_virtual(f, "apply", desc, &[v])?.r();
                if r != 0 {
                    let inner = take(vm, r)?;
                    if let Op::FlatMap { inner: slot, .. } = &mut s.ops[k - 1] {
                        *slot = Some(Box::new(inner));
                    }
                }
            }
            Op::Skip(n) => {
                if n > 0 {
                    s.ops[k - 1] = Op::Skip(n - 1);
                } else {
                    return Ok(Some(v));
                }
            }
            Op::Distinct(seen) => {
                let mut dup = false;
                for x in &seen {
                    if same_element(vm, *x, v, in_kind)? {
                        dup = true;
                        break;
                    }
                }
                if !dup {
                    if let Op::Distinct(seen) = &mut s.ops[k - 1] {
                        seen.push(v);
                    }
                    return Ok(Some(v));
                }
            }
            Op::TakeWhile { f, desc, .. } => {
                if vm.invoke_virtual(f, "test", desc, &[v])?.z() {
                    return Ok(Some(v));
                }
                s.ops[k - 1] = Op::TakeWhile { f, desc, done: true };
                return Ok(None);
            }
            Op::DropWhile { f, desc, dropping } => {
                if dropping && vm.invoke_virtual(f, "test", desc, &[v])?.z() {
                    continue;
                }
                s.ops[k - 1] = Op::DropWhile { f, desc, dropping: false };
                return Ok(Some(v));
            }
            Op::Box(kind) => return Ok(Some(Value::Ref(super::lang::box_value(vm, v, kind)?))),
            Op::Widen(to) => return Ok(Some(super::lang::widen(v, to))),
            Op::Limit(_) | Op::Sorted { .. } => unreachable!(),
        }
    }
}


/// The element kind entering operation `i`.
fn kind_before(s: &JStream, i: usize) -> u8 {
    let mut kind = s.src_kind;
    for op in &s.ops[..i] {
        kind = match op {
            Op::Map { out, .. } | Op::FlatMap { out, .. } => *out,
            Op::Box(_) => b'L',
            Op::Widen(to) => *to,
            _ => kind,
        };
    }
    kind
}

fn same_element(vm: &mut Vm, a: Value, b: Value, kind: u8) -> Res<bool> {
    Ok(match kind {
        b'D' => super::boxes::double_to_long_bits(a.d()) == super::boxes::double_to_long_bits(b.d()),
        b'L' => equals(vm, a, b)?,
        _ => a == b,
    })
}

/// `Double.compare`: numeric order, -0.0 before 0.0, NaN last.
fn java_double_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    if a < b {
        std::cmp::Ordering::Less
    } else if a > b {
        std::cmp::Ordering::Greater
    } else {
        super::boxes::double_to_long_bits(a).cmp(&super::boxes::double_to_long_bits(b))
    }
}

fn sort_kind(vm: &mut Vm, items: &mut [Value], kind: u8, cmp: ObjId) -> Res<()> {
    match kind {
        b'I' => items.sort_by_key(|v| v.i()),
        b'J' => items.sort_by_key(|v| v.j()),
        b'D' => items.sort_by(|a, b| java_double_cmp(a.d(), b.d())),
        _ => sort_values(vm, items, cmp)?,
    }
    Ok(())
}

// ------------------------------------------------------------------ intermediate

/// A new pipeline object: this one's with one more operation.
fn with_op(vm: &mut Vm, a: &[Value], op: Op) -> Res<Value> {
    let t = this(vm, a)?;
    let mut s = take(vm, t)?;
    s.kind = match &op {
        Op::Map { out, .. } | Op::FlatMap { out, .. } => *out,
        Op::Box(_) => b'L',
        Op::Widen(to) => *to,
        _ => s.kind,
    };
    s.ops.push(op);
    new_stream(vm, s)
}

fn kind_of(vm: &Vm, r: ObjId) -> u8 {
    match state(vm, r) {
        Some(State::Stream(s)) => s.kind,
        _ => b'L',
    }
}

fn intermediate(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let kind = kind_of(vm, t);
    let f = arg(a, 1).r();
    let needs_fn = !matches!(op, "distinct" | "sorted" | "boxed" | "asLongStream" | "asDoubleStream" | "sequential" | "parallel" | "unordered" | "limit" | "skip" | "onClose");
    if needs_fn && f == 0 && !(op == "sorted" && a.len() > 1) {
        return Err(vm.npe(None));
    }
    let new = match op {
        "map" => {
            let (name, desc) = unary(kind);
            Op::Map { f, name, desc, out: kind }
        }
        "mapToObj" => Op::Map { f, name: "apply", desc: to_obj(kind), out: b'L' },
        "mapToInt" | "mapToLong" | "mapToDouble" => {
            let to = match op {
                "mapToInt" => b'I',
                "mapToLong" => b'J',
                _ => b'D',
            };
            let (name, desc) = convert(kind, to);
            Op::Map { f, name, desc, out: to }
        }
        "filter" => Op::Filter { f, desc: predicate(kind) },
        "peek" => Op::Peek { f, desc: consumer(kind) },
        "flatMap" => Op::FlatMap { f, desc: to_obj(kind), inner: None, out: kind },
        "flatMapToInt" => Op::FlatMap { f, desc: to_obj(kind), inner: None, out: b'I' },
        "flatMapToObj" => Op::FlatMap { f, desc: to_obj(kind), inner: None, out: b'L' },
        "limit" | "skip" => {
            let n = arg(a, 1).j();
            if n < 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&n.to_string())));
            }
            if op == "limit" { Op::Limit(n) } else { Op::Skip(n) }
        }
        "distinct" => Op::Distinct(Vec::new()),
        "sorted" => Op::Sorted { cmp: arg(a, 1).r(), buf: None, pos: 0 },
        "takeWhile" => Op::TakeWhile { f, desc: predicate(kind), done: false },
        "dropWhile" => Op::DropWhile { f, desc: predicate(kind), dropping: true },
        "boxed" => Op::Box(kind),
        "asLongStream" => Op::Widen(b'J'),
        "asDoubleStream" => Op::Widen(b'D'),
        "sequential" | "parallel" | "unordered" => return ret_ref(t),
        "onClose" => {
            if let Some(State::Stream(s)) = super::state_mut(vm, t) {
                s.close_handlers.push(f);
            }
            return ret_ref(t);
        }
        _ => return Err(Exc::Unsupported(format!("Stream.{op}"))),
    };
    with_op(vm, a, new)
}

// ------------------------------------------------------------------ terminal

/// Kahan summation with Java's compensation (`Collectors.sumWithCompensation`).
#[derive(Default, Clone, Copy, Debug)]
pub struct Kahan {
    sum: f64,
    comp: f64,
    simple: f64,
}

impl Kahan {
    fn add(&mut self, v: f64) {
        let tmp = v - self.comp;
        let velvel = self.sum + tmp;
        self.comp = (velvel - self.sum) - tmp;
        self.sum = velvel;
        self.simple += v;
    }
    fn total(&self) -> f64 {
        let tmp = self.sum - self.comp;
        if tmp.is_nan() && self.simple.is_infinite() { self.simple } else { tmp }
    }
}

fn optional_kind(vm: &mut Vm, kind: u8, v: Option<Value>) -> Res<Value> {
    let class = match kind {
        b'I' => "java/util/OptionalInt",
        b'J' => "java/util/OptionalLong",
        b'D' => "java/util/OptionalDouble",
        _ => return util::optional_of(vm, v.unwrap_or(Value::NULL)),
    };
    let zero = match kind {
        b'J' => Value::Long(0),
        b'D' => Value::Double(0.0),
        _ => Value::Int(0),
    };
    new_optional(vm, class, v.unwrap_or(zero), v.is_some())
}

fn run_close(vm: &mut Vm, s: &JStream) -> Res<()> {
    for h in s.close_handlers.clone() {
        vm.invoke_virtual(h, "run", "()V", &[])?;
    }
    Ok(())
}

fn terminal(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    if op == "close" {
        if let Some(State::Stream(s)) = state(vm, t).cloned().as_ref() {
            let s = (**s).clone();
            if let Some(State::Stream(x)) = super::state_mut(vm, t) {
                x.close_handlers.clear();
            }
            run_close(vm, &s)?;
        }
        return void();
    }
    if op == "isParallel" {
        return ret_bool(false);
    }
    let mut s = take(vm, t)?;
    let kind = s.kind;
    match op {
        "forEach" | "forEachOrdered" => {
            let f = arg(a, 1).r();
            if f == 0 {
                return Err(vm.npe(None));
            }
            while let Some(v) = pull(vm, &mut s)? {
                vm.invoke_virtual(f, "accept", consumer(kind), &[v])?;
            }
            void()
        }
        "count" => {
            if let Some(n) = s.known_size() {
                return Ok(Value::Long(n));
            }
            let mut n = 0i64;
            while pull(vm, &mut s)?.is_some() {
                n += 1;
            }
            Ok(Value::Long(n))
        }
        "toList" | "toArray" | "toArrayTyped" => {
            let mut items = Vec::new();
            while let Some(v) = pull(vm, &mut s)? {
                items.push(v);
            }
            match op {
                "toList" => util::immutable_list_allowing_nulls(vm, items),
                "toArrayTyped" => {
                    let arr = vm.invoke_virtual(arg(a, 1).r(), "apply", "(I)Ljava/lang/Object;", &[Value::Int(items.len() as i32)])?.r();
                    let Some(len) = vm.array(arr).map(Array::len) else { return Err(vm.npe(None)) };
                    if len < items.len() {
                        return Err(vm.throw_new("java/lang/IllegalStateException", Some("Begin size 0 is not equal to fixed size")));
                    }
                    for (i, v) in items.iter().enumerate() {
                        crate::vm::jdk::lang::array_store(vm, arr, i, *v)?;
                    }
                    ret_ref(arr)
                }
                _ => {
                    let desc = match kind {
                        b'I' => "I",
                        b'J' => "J",
                        b'D' => "D",
                        _ => "Ljava/lang/Object;",
                    };
                    let arr = vm.new_array_of(desc, items.len() as i32)?;
                    if let Some(arr) = vm.array_mut(arr) {
                        for (i, v) in items.into_iter().enumerate() {
                            arr.set(i, v);
                        }
                    }
                    ret_ref(arr)
                }
            }
        }
        "reduceIdentity" | "reduceCombiner" => {
            let mut acc = arg(a, 1);
            let f = arg(a, 2).r();
            let (name, desc) = if op == "reduceCombiner" { ("apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") } else { binary(kind) };
            while let Some(v) = pull(vm, &mut s)? {
                acc = vm.invoke_virtual(f, name, desc, &[acc, v])?;
            }
            Ok(acc)
        }
        "reduce" => {
            let f = arg(a, 1).r();
            let (name, desc) = binary(kind);
            let mut acc: Option<Value> = None;
            while let Some(v) = pull(vm, &mut s)? {
                acc = Some(match acc {
                    None => v,
                    Some(x) => vm.invoke_virtual(f, name, desc, &[x, v])?,
                });
            }
            if kind == b'L' && acc.is_some_and(|v| v.r() == 0) {
                return Err(vm.npe(None));
            }
            optional_kind(vm, kind, acc)
        }
        "min" | "max" => {
            let cmp = arg(a, 1).r();
            let mut best: Option<Value> = None;
            while let Some(v) = pull(vm, &mut s)? {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let c = match kind {
                            b'I' => v.i().cmp(&b.i()) as i32,
                            b'J' => v.j().cmp(&b.j()) as i32,
                            b'D' => {
                                // Math.min/max: NaN wins, -0.0 is below 0.0.
                                if v.d().is_nan() || b.d().is_nan() {
                                    best = Some(Value::Double(f64::NAN));
                                    continue;
                                }
                                java_double_cmp(v.d(), b.d()) as i32
                            }
                            _ => compare(vm, v, b, cmp)?,
                        };
                        if (op == "max" && c > 0) || (op == "min" && c < 0) { v } else { b }
                    }
                });
            }
            if kind == b'L' && best.is_some_and(|v| v.r() == 0) {
                return Err(vm.npe(None));
            }
            optional_kind(vm, kind, best)
        }
        "sum" => {
            let mut k = Kahan::default();
            let (mut i, mut j) = (0i32, 0i64);
            while let Some(v) = pull(vm, &mut s)? {
                match v {
                    Value::Int(x) => i = i.wrapping_add(x),
                    Value::Long(x) => j = j.wrapping_add(x),
                    Value::Double(x) => k.add(x),
                    _ => {}
                }
            }
            Ok(match kind {
                b'I' => Value::Int(i),
                b'J' => Value::Long(j),
                _ => Value::Double(k.total()),
            })
        }
        "average" => {
            let mut k = Kahan::default();
            let (mut sum, mut n) = (0i64, 0i64);
            while let Some(v) = pull(vm, &mut s)? {
                n += 1;
                match v {
                    Value::Int(x) => sum += x as i64,
                    Value::Long(x) => sum = sum.wrapping_add(x),
                    Value::Double(x) => k.add(x),
                    _ => {}
                }
            }
            let avg = if n == 0 {
                None
            } else if kind == b'D' {
                Some(Value::Double(k.total() / n as f64))
            } else {
                Some(Value::Double(sum as f64 / n as f64))
            };
            optional_kind(vm, b'D', avg)
        }
        "summaryStatistics" => {
            let stats = new_stats(vm, kind)?;
            while let Some(v) = pull(vm, &mut s)? {
                stats_accept(vm, stats, v)?;
            }
            ret_ref(stats)
        }
        "findFirst" | "findAny" => {
            let v = pull(vm, &mut s)?;
            if kind == b'L' && v.is_some_and(|v| v.r() == 0) {
                return Err(vm.npe(None));
            }
            optional_kind(vm, kind, v)
        }
        "anyMatch" | "allMatch" | "noneMatch" => {
            let f = arg(a, 1).r();
            while let Some(v) = pull(vm, &mut s)? {
                let hit = vm.invoke_virtual(f, "test", predicate(kind), &[v])?.z();
                match (op, hit) {
                    ("anyMatch", true) => return ret_bool(true),
                    ("allMatch", false) | ("noneMatch", true) => return ret_bool(false),
                    _ => {}
                }
            }
            ret_bool(op != "anyMatch")
        }
        "collect" => {
            let c = arg(a, 1).r();
            if c == 0 {
                return Err(vm.npe(None));
            }
            let mut acc = acc_new(vm, c)?;
            while let Some(v) = pull(vm, &mut s)? {
                acc_add(vm, c, &mut acc, v)?;
            }
            acc_finish(vm, c, acc)
        }
        "collect3" => {
            let container = vm.invoke_virtual(arg(a, 1).r(), "get", "()Ljava/lang/Object;", &[])?;
            let desc = match kind {
                b'I' => "(Ljava/lang/Object;I)V",
                b'J' => "(Ljava/lang/Object;J)V",
                b'D' => "(Ljava/lang/Object;D)V",
                _ => "(Ljava/lang/Object;Ljava/lang/Object;)V",
            };
            while let Some(v) = pull(vm, &mut s)? {
                vm.invoke_virtual(arg(a, 2).r(), "accept", desc, &[container, v])?;
            }
            Ok(container)
        }
        "iterator" => {
            let c = vm.load("java/util/stream/StreamIterator")?;
            let r = vm.new_instance(c)?;
            if let Some(o) = vm.obj_mut(r) {
                o.data = Data::Jdk(Box::new(State::Stream(Box::new(s))));
            }
            ret_ref(r)
        }
        _ => Err(Exc::Unsupported(format!("Stream.{op}"))),
    }
}

fn iterator_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(State::Stream(s)) = state(vm, t) else { return Err(Exc::Internal("not a stream iterator".into())) };
    let mut s = (**s).clone();
    if s.peeked.is_none() {
        s.peeked = Some(pull(vm, &mut s)?);
    }
    let next = s.peeked.unwrap_or(None);
    if op != "hasNext" {
        s.peeked = None;
    }
    let kind = s.kind;
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Stream(Box::new(s))));
    }
    match op {
        "hasNext" => ret_bool(next.is_some()),
        _ => match next {
            Some(v) if kind != b'L' && op == "next" => Ok(Value::Ref(super::lang::box_value(vm, v, kind)?)),
            Some(v) => Ok(v),
            None => Err(vm.throw_new("java/util/NoSuchElementException", None)),
        },
    }
}

// ------------------------------------------------------------------ sources

fn of_array(vm: &mut Vm, arr: ObjId, kind: u8) -> Res<Value> {
    if arr == 0 {
        return Err(vm.npe(None));
    }
    let items = vm.array_values(arr);
    new_stream(vm, JStream::new(Src::Items(items, 0), kind))
}

fn statics(vm: &mut Vm, a: &[Value], op: &str, kind: u8) -> Res<Value> {
    match op {
        "of1" => new_stream(vm, JStream::new(Src::Items(vec![arg(a, 0)], 0), kind)),
        "ofArray" => of_array(vm, arg(a, 0).r(), kind),
        "ofNullable" => {
            let items = if arg(a, 0).r() == 0 { Vec::new() } else { vec![arg(a, 0)] };
            new_stream(vm, JStream::new(Src::Items(items, 0), kind))
        }
        "empty" => new_stream(vm, JStream::new(Src::Items(Vec::new(), 0), kind)),
        "range" | "rangeClosed" => {
            let (from, to) = match kind {
                b'J' => (arg(a, 0).j(), arg(a, 1).j()),
                _ => (arg(a, 0).i() as i64, arg(a, 1).i() as i64),
            };
            let end = if op == "rangeClosed" { to + 1 } else { to };
            new_stream(vm, JStream::new(Src::Range(from, end.max(from)), kind))
        }
        "iterate" => new_stream(vm, JStream::new(Src::Iterate { cur: arg(a, 0), next: arg(a, 1).r(), has_next: 0, started: false, done: false }, kind)),
        "iterate3" => new_stream(vm, JStream::new(Src::Iterate { cur: arg(a, 0), next: arg(a, 2).r(), has_next: arg(a, 1).r(), started: false, done: false }, kind)),
        "generate" => new_stream(vm, JStream::new(Src::Generate(arg(a, 0).r()), kind)),
        "concat" => {
            let (x, y) = (arg(a, 0).r(), arg(a, 1).r());
            if x == 0 || y == 0 {
                return Err(vm.npe(None));
            }
            let (p, q) = (take(vm, x)?, take(vm, y)?);
            let mut s = JStream::new(Src::Concat(Box::new(p.clone()), Box::new(q.clone()), false), kind);
            s.close_handlers = p.close_handlers.into_iter().chain(q.close_handlers).collect();
            new_stream(vm, s)
        }
        _ => Err(Exc::Unsupported(format!("Stream.{op}"))),
    }
}

/// A stream over any `Iterable`'s iterator, lazily (`StreamSupport`, `Spliterator` users).
pub fn stream_of_iterator(vm: &mut Vm, it: ObjId) -> Res<Value> {
    new_stream(vm, JStream::new(Src::Iter(it), b'L'))
}

// ------------------------------------------------------------------ summary statistics

fn new_stats(vm: &mut Vm, kind: u8) -> Res<ObjId> {
    let class = match kind {
        b'I' => "java/util/IntSummaryStatistics",
        b'J' => "java/util/LongSummaryStatistics",
        _ => "java/util/DoubleSummaryStatistics",
    };
    let c = vm.load(class)?;
    let r = vm.new_instance(c)?;
    stats_init(vm, r, kind);
    Ok(r)
}

fn stats_init(vm: &mut Vm, r: ObjId, kind: u8) {
    let (min, max) = match kind {
        b'I' => (Value::Int(i32::MAX), Value::Int(i32::MIN)),
        b'J' => (Value::Long(i64::MAX), Value::Long(i64::MIN)),
        _ => (Value::Double(f64::INFINITY), Value::Double(f64::NEG_INFINITY)),
    };
    if let Some(o) = vm.obj_mut(r) {
        // count, integer sum, min, max, kind, Kahan sum parts.
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Long(0), Value::Long(0), min, max, Value::Int(kind as i32), Value::Double(0.0), Value::Double(0.0), Value::Double(0.0)])));
    }
}

fn stats(vm: &Vm, r: ObjId) -> Vec<Value> {
    match state(vm, r) {
        Some(State::Generic(v)) if v.len() == 8 => v.clone(),
        _ => vec![Value::Long(0), Value::Long(0), Value::Int(0), Value::Int(0), Value::Int(b'I' as i32), Value::Double(0.0), Value::Double(0.0), Value::Double(0.0)],
    }
}

fn stats_accept(vm: &mut Vm, r: ObjId, v: Value) -> Res<()> {
    let mut s = stats(vm, r);
    s[0] = Value::Long(s[0].j() + 1);
    match v {
        Value::Int(x) => {
            s[1] = Value::Long(s[1].j() + x as i64);
            s[2] = Value::Int(s[2].i().min(x));
            s[3] = Value::Int(s[3].i().max(x));
        }
        Value::Long(x) => {
            s[1] = Value::Long(s[1].j().wrapping_add(x));
            s[2] = Value::Long(s[2].j().min(x));
            s[3] = Value::Long(s[3].j().max(x));
        }
        Value::Double(x) => {
            let mut k = Kahan { sum: s[5].d(), comp: s[6].d(), simple: s[7].d() };
            k.add(x);
            s[5] = Value::Double(k.sum);
            s[6] = Value::Double(k.comp);
            s[7] = Value::Double(k.simple);
            s[2] = Value::Double(java_min(s[2].d(), x));
            s[3] = Value::Double(java_max(s[3].d(), x));
        }
        _ => {}
    }
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(s)));
    }
    Ok(())
}

fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}

fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}

fn stats_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let s = stats(vm, t);
    let kind = s[4].i() as u8;
    let count = s[0].j();
    let dsum = Kahan { sum: s[5].d(), comp: s[6].d(), simple: s[7].d() }.total();
    let average = if count == 0 { 0.0 } else if kind == b'D' { dsum / count as f64 } else { s[1].j() as f64 / count as f64 };
    match op {
        "init" => {
            stats_init(vm, t, kind);
            void()
        }
        "accept" => {
            let v = match (kind, arg(a, 1)) {
                (b'J', Value::Int(x)) => Value::Long(x as i64),
                (_, v) => v,
            };
            stats_accept(vm, t, v)?;
            void()
        }
        "combine" => {
            let o = stats(vm, arg(a, 1).r());
            let mut s2 = s.clone();
            s2[0] = Value::Long(s[0].j() + o[0].j());
            s2[1] = Value::Long(s[1].j() + o[1].j());
            match kind {
                b'I' => {
                    s2[2] = Value::Int(s[2].i().min(o[2].i()));
                    s2[3] = Value::Int(s[3].i().max(o[3].i()));
                }
                b'J' => {
                    s2[2] = Value::Long(s[2].j().min(o[2].j()));
                    s2[3] = Value::Long(s[3].j().max(o[3].j()));
                }
                _ => {
                    let mut k = Kahan { sum: s[5].d(), comp: s[6].d(), simple: s[7].d() };
                    k.add(o[5].d());
                    k.add(-o[6].d());
                    s2[5] = Value::Double(k.sum);
                    s2[6] = Value::Double(k.comp);
                    s2[7] = Value::Double(s[7].d() + o[7].d());
                    s2[2] = Value::Double(java_min(s[2].d(), o[2].d()));
                    s2[3] = Value::Double(java_max(s[3].d(), o[3].d()));
                }
            }
            if let Some(obj) = vm.obj_mut(t) {
                obj.data = Data::Jdk(Box::new(State::Generic(s2)));
            }
            void()
        }
        "getCount" => Ok(Value::Long(count)),
        "getSum" => Ok(match kind {
            b'D' => Value::Double(dsum),
            _ => Value::Long(s[1].j()),
        }),
        "getMin" => Ok(s[2]),
        "getMax" => Ok(s[3]),
        "getAverage" => Ok(Value::Double(average)),
        "toString" => {
            let name = match kind {
                b'I' => "IntSummaryStatistics",
                b'J' => "LongSummaryStatistics",
                _ => "DoubleSummaryStatistics",
            };
            let f = |v: f64| format_f(v);
            let text = match kind {
                b'D' => format!("{name}{{count={count}, sum={}, min={}, average={}, max={}}}", f(dsum), f(s[2].d()), f(average), f(s[3].d())),
                b'I' => format!("{name}{{count={count}, sum={}, min={}, average={}, max={}}}", s[1].j(), s[2].i(), f(average), s[3].i()),
                _ => format!("{name}{{count={count}, sum={}, min={}, average={}, max={}}}", s[1].j(), s[2].j(), f(average), s[3].j()),
            };
            ret_str(vm, &text)
        }
        _ => Err(Exc::Unsupported(format!("SummaryStatistics.{op}"))),
    }
}

/// `String.format("%f", v)`: six decimals, half-up.
fn format_f(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    super::string::fixed_point(v, 6)
}

// ------------------------------------------------------------------ collectors

/// A collector's accumulation so far.
#[derive(Debug, Clone)]
pub enum Acc {
    Items(Vec<Value>),
    Count(i64),
    SumI(i64),
    SumD(Kahan),
    Avg(i64, i64),
    AvgD(Kahan, i64),
    /// A container object (a collection, a map, a custom collector's).
    Container(Value),
    /// `groupingBy`/`partitioningBy`: the map, and each group's accumulation
    /// (the map holds the group's index).
    Group(ObjId, Vec<Acc>),
    /// `teeing`.
    Pair(Box<Acc>, Box<Acc>),
    /// A custom collector: its container and accumulator.
    Custom(Value, ObjId),
    Best(Option<Value>),
    Reduce(Option<Value>),
}

/// A native collector's kind and arguments.
fn spec(vm: &Vm, c: ObjId) -> Option<(i32, Vec<Value>)> {
    if vm.class_of(c).is_some_and(|k| vm.class_name(k) != "java/util/stream/Collectors$CollectorImpl") {
        return None;
    }
    match state(vm, c) {
        Some(State::Generic(v)) if !v.is_empty() => Some((v[0].i(), v[1..].to_vec())),
        _ => None,
    }
}

mod kind {
    pub const TO_LIST: i32 = 0;
    pub const TO_SET: i32 = 1;
    pub const TO_MAP: i32 = 2;
    pub const JOINING: i32 = 3;
    pub const GROUPING: i32 = 4;
    pub const COUNTING: i32 = 5;
    pub const SUMMING: i32 = 6;
    pub const AVERAGING: i32 = 7;
    pub const MAPPING: i32 = 8;
    pub const FILTERING: i32 = 9;
    pub const PARTITIONING: i32 = 10;
    pub const TO_COLLECTION: i32 = 11;
    pub const REDUCING: i32 = 12;
    pub const MIN_BY: i32 = 13;
    pub const MAX_BY: i32 = 14;
    pub const AND_THEN: i32 = 15;
    pub const UNMODIFIABLE_LIST: i32 = 16;
    pub const UNMODIFIABLE_SET: i32 = 17;
    pub const UNMODIFIABLE_MAP: i32 = 18;
    pub const OF: i32 = 19;
    pub const TEEING: i32 = 20;
    pub const FLAT_MAPPING: i32 = 21;
    pub const SUMMARIZING: i32 = 22;
}

fn new_collector(vm: &mut Vm, k: i32, args: &[Value]) -> Res<Value> {
    let c = vm.load("java/util/stream/Collectors$CollectorImpl")?;
    let r = vm.new_instance(c)?;
    let mut v = vec![Value::Int(k)];
    v.extend_from_slice(args);
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(v)));
    }
    ret_ref(r)
}

fn acc_new(vm: &mut Vm, c: ObjId) -> Res<Acc> {
    let Some((k, args)) = spec(vm, c) else {
        let supplier = vm.invoke_virtual(c, "supplier", "()Ljava/util/function/Supplier;", &[])?.r();
        let container = vm.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])?;
        let accumulator = vm.invoke_virtual(c, "accumulator", "()Ljava/util/function/BiConsumer;", &[])?.r();
        return Ok(Acc::Custom(container, accumulator));
    };
    Ok(match k {
        kind::TO_MAP | kind::UNMODIFIABLE_MAP => {
            let supplier = args.get(3).map_or(0, |v| v.r());
            let map = if supplier != 0 { vm.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])? } else { Value::Ref(util::new_map(vm, "java/util/HashMap")?) };
            Acc::Container(map)
        }
        kind::TO_SET | kind::UNMODIFIABLE_SET => {
            let set = util::new_collection(vm, "java/util/HashSet", State::Map(Box::new({
                let mut m = util::JMap::new();
                m.set = true;
                m
            })))?;
            Acc::Container(Value::Ref(set))
        }
        kind::TO_COLLECTION => Acc::Container(vm.invoke_virtual(args[0].r(), "get", "()Ljava/lang/Object;", &[])?),
        kind::OF => Acc::Custom(vm.invoke_virtual(args[0].r(), "get", "()Ljava/lang/Object;", &[])?, args[1].r()),
        kind::GROUPING => {
            let factory = args[1].r();
            let map = if factory != 0 { vm.invoke_virtual(factory, "get", "()Ljava/lang/Object;", &[])?.r() } else { util::new_map(vm, "java/util/HashMap")? };
            Acc::Group(map, Vec::new())
        }
        kind::PARTITIONING => {
            let map = util::new_map(vm, "java/util/LinkedHashMap")?;
            let down = args[1].r();
            let mut accs = Vec::new();
            for (i, b) in [0, 1].into_iter().enumerate() {
                let key = super::lang::box_value(vm, Value::Int(b), b'Z')?;
                map_put(vm, map, Value::Ref(key), Value::Int(i as i32))?;
                accs.push(acc_new(vm, down)?);
            }
            Acc::Group(map, accs)
        }
        kind::COUNTING => Acc::Count(0),
        kind::SUMMING => match args[1].i() as u8 {
            b'D' => Acc::SumD(Kahan::default()),
            _ => Acc::SumI(0),
        },
        kind::AVERAGING => match args[1].i() as u8 {
            b'D' => Acc::AvgD(Kahan::default(), 0),
            _ => Acc::Avg(0, 0),
        },
        kind::MAPPING | kind::FILTERING | kind::FLAT_MAPPING => acc_new(vm, args[1].r())?,
        kind::AND_THEN => acc_new(vm, args[0].r())?,
        kind::TEEING => Acc::Pair(Box::new(acc_new(vm, args[0].r())?), Box::new(acc_new(vm, args[1].r())?)),
        kind::MIN_BY | kind::MAX_BY => Acc::Best(None),
        kind::REDUCING => Acc::Reduce(if args[0].i() != 0 { Some(args[1]) } else { None }),
        kind::SUMMARIZING => Acc::Container(Value::Ref(new_stats(vm, args[1].i() as u8)?)),
        _ => Acc::Items(Vec::new()),
    })
}

fn acc_add(vm: &mut Vm, c: ObjId, acc: &mut Acc, v: Value) -> Res<()> {
    let Some((k, args)) = spec(vm, c) else {
        if let Acc::Custom(container, accumulator) = acc {
            vm.invoke_virtual(*accumulator, "accept", "(Ljava/lang/Object;Ljava/lang/Object;)V", &[*container, v])?;
        }
        return Ok(());
    };
    match (k, acc) {
        (kind::TO_MAP | kind::UNMODIFIABLE_MAP, Acc::Container(map)) => {
            let key = vm.invoke_virtual(args[0].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?;
            let value = vm.invoke_virtual(args[1].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?;
            if value.r() == 0 {
                return Err(vm.npe(None));
            }
            let merge = args.get(2).map_or(0, |m| m.r());
            match map_get_any(vm, map.r(), key)? {
                Some(old) if old.r() != 0 => {
                    if merge == 0 {
                        let (ks, os, vs) = (text(vm, key)?, text(vm, old)?, text(vm, value)?);
                        let msg = format!("Duplicate key {ks} (attempted merging values {os} and {vs})");
                        return Err(vm.throw_new("java/lang/IllegalStateException", Some(&msg)));
                    }
                    let merged = vm.invoke_virtual(merge, "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[old, value])?;
                    map_put_any(vm, map.r(), key, merged)?;
                }
                _ => {
                    map_put_any(vm, map.r(), key, value)?;
                }
            }
        }
        (kind::TO_SET | kind::UNMODIFIABLE_SET | kind::TO_COLLECTION, Acc::Container(coll)) => {
            vm.invoke_virtual(coll.r(), "add", "(Ljava/lang/Object;)Z", &[v])?;
        }
        (kind::OF, Acc::Custom(container, accumulator)) => {
            vm.invoke_virtual(*accumulator, "accept", "(Ljava/lang/Object;Ljava/lang/Object;)V", &[*container, v])?;
        }
        (kind::GROUPING | kind::PARTITIONING, Acc::Group(map, accs)) => {
            let key = if k == kind::GROUPING {
                let key = vm.invoke_virtual(args[0].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?;
                if key.r() == 0 {
                    return Err(vm.throw_new("java/lang/NullPointerException", Some("element cannot be mapped to a null key")));
                }
                key
            } else {
                let hit = vm.invoke_virtual(args[0].r(), "test", "(Ljava/lang/Object;)Z", &[v])?.z();
                Value::Ref(super::lang::box_value(vm, Value::Int(hit as i32), b'Z')?)
            };
            let down = if k == kind::GROUPING { args[2].r() } else { args[1].r() };
            let index = match map_get_any(vm, *map, key)? {
                Some(Value::Int(i)) => i as usize,
                _ => {
                    accs.push(acc_new(vm, down)?);
                    map_put_any(vm, *map, key, Value::Int(accs.len() as i32 - 1))?;
                    accs.len() - 1
                }
            };
            acc_add(vm, down, &mut accs[index], v)?;
        }
        (kind::COUNTING, Acc::Count(n)) => *n += 1,
        (kind::SUMMING, a) => {
            let (name, desc) = key_fn(args[1].i() as u8);
            let x = vm.invoke_virtual(args[0].r(), name, desc, &[v])?;
            match (a, x) {
                (Acc::SumI(s), Value::Int(i)) => *s += i as i64,
                (Acc::SumI(s), Value::Long(j)) => *s = s.wrapping_add(j),
                (Acc::SumD(k), Value::Double(d)) => k.add(d),
                _ => {}
            }
        }
        (kind::AVERAGING, a) => {
            let (name, desc) = key_fn(args[1].i() as u8);
            let x = vm.invoke_virtual(args[0].r(), name, desc, &[v])?;
            match (a, x) {
                (Acc::Avg(s, n), Value::Int(i)) => {
                    *s += i as i64;
                    *n += 1;
                }
                (Acc::Avg(s, n), Value::Long(j)) => {
                    *s = s.wrapping_add(j);
                    *n += 1;
                }
                (Acc::AvgD(k, n), Value::Double(d)) => {
                    k.add(d);
                    *n += 1;
                }
                _ => {}
            }
        }
        (kind::SUMMARIZING, Acc::Container(stats)) => {
            let (name, desc) = key_fn(args[1].i() as u8);
            let x = vm.invoke_virtual(args[0].r(), name, desc, &[v])?;
            stats_accept(vm, stats.r(), x)?;
        }
        (kind::MAPPING, a) => {
            let m = vm.invoke_virtual(args[0].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?;
            acc_add(vm, args[1].r(), a, m)?;
        }
        (kind::FILTERING, a) => {
            if vm.invoke_virtual(args[0].r(), "test", "(Ljava/lang/Object;)Z", &[v])?.z() {
                acc_add(vm, args[1].r(), a, v)?;
            }
        }
        (kind::FLAT_MAPPING, a) => {
            let st = vm.invoke_virtual(args[0].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])?.r();
            if st != 0 {
                let mut inner = take(vm, st)?;
                while let Some(x) = pull(vm, &mut inner)? {
                    acc_add(vm, args[1].r(), a, x)?;
                }
            }
        }
        (kind::AND_THEN, a) => acc_add(vm, args[0].r(), a, v)?,
        (kind::TEEING, Acc::Pair(x, y)) => {
            acc_add(vm, args[0].r(), x, v)?;
            acc_add(vm, args[1].r(), y, v)?;
        }
        (kind::MIN_BY | kind::MAX_BY, Acc::Best(best)) => {
            *best = Some(match *best {
                None => v,
                Some(b) => {
                    let c = compare(vm, b, v, args[0].r())?;
                    // BinaryOperator.minBy/maxBy keep the first of equals.
                    if (k == kind::MIN_BY && c <= 0) || (k == kind::MAX_BY && c >= 0) { b } else { v }
                }
            });
        }
        (kind::REDUCING, Acc::Reduce(r)) => {
            let mapped = if args[2].r() != 0 { vm.invoke_virtual(args[2].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[v])? } else { v };
            *r = Some(match *r {
                None => mapped,
                Some(x) => vm.invoke_virtual(args[3].r(), "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[x, mapped])?,
            });
        }
        (_, Acc::Items(items)) => items.push(v),
        (k, a) => return Err(Exc::Internal(format!("collector {k} with {a:?}"))),
    }
    Ok(())
}

fn acc_finish(vm: &mut Vm, c: ObjId, acc: Acc) -> Res<Value> {
    let Some((k, args)) = spec(vm, c) else {
        let Acc::Custom(container, _) = acc else { return Ok(Value::NULL) };
        if custom_identity_finish(vm, c)? {
            return Ok(container);
        }
        let finisher = vm.invoke_virtual(c, "finisher", "()Ljava/util/function/Function;", &[])?.r();
        return vm.invoke_virtual(finisher, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[container]);
    };
    Ok(match (k, acc) {
        (kind::TO_LIST, Acc::Items(items)) => Value::Ref(new_list(vm, items)?),
        (kind::UNMODIFIABLE_LIST, Acc::Items(items)) => util::immutable_list(vm, items)?,
        (kind::UNMODIFIABLE_SET, Acc::Container(set)) => {
            let items = iterate(vm, set.r())?;
            util::immutable_set(vm, items)?
        }
        (kind::UNMODIFIABLE_MAP, Acc::Container(map)) => {
            let pairs = util::map_pairs(vm, map.r())?;
            util::immutable_map(vm, pairs)?
        }
        (kind::JOINING, Acc::Items(items)) => {
            let mut out = super::string::units(vm, args[1].r())?;
            let delim = super::string::units(vm, args[0].r())?;
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(&delim);
                }
                out.extend(vm.stringify(*v, "Ljava/lang/Object;")?);
            }
            out.extend(super::string::units(vm, args[2].r())?);
            Value::Ref(vm.new_string_units(out)?)
        }
        (kind::GROUPING | kind::PARTITIONING, Acc::Group(map, accs)) => {
            let down = if k == kind::GROUPING { args[2].r() } else { args[1].r() };
            let pairs = util::map_pairs(vm, map)?;
            let mut finished = Vec::with_capacity(accs.len());
            for a in accs {
                finished.push(acc_finish(vm, down, a)?);
            }
            for (key, index) in pairs {
                let v = finished.get(index.i() as usize).copied().unwrap_or(Value::NULL);
                map_put_any(vm, map, key, v)?;
            }
            Value::Ref(map)
        }
        (kind::COUNTING, Acc::Count(n)) => Value::Ref(super::lang::box_value(vm, Value::Long(n), b'J')?),
        (kind::SUMMING, Acc::SumI(s)) => match args[1].i() as u8 {
            b'I' => Value::Ref(super::lang::box_value(vm, Value::Int(s as i32), b'I')?),
            _ => Value::Ref(super::lang::box_value(vm, Value::Long(s), b'J')?),
        },
        (kind::SUMMING, Acc::SumD(kh)) => Value::Ref(super::lang::box_value(vm, Value::Double(kh.total()), b'D')?),
        (kind::AVERAGING, Acc::Avg(s, n)) => Value::Ref(super::lang::box_value(vm, Value::Double(if n == 0 { 0.0 } else { s as f64 / n as f64 }), b'D')?),
        (kind::AVERAGING, Acc::AvgD(kh, n)) => Value::Ref(super::lang::box_value(vm, Value::Double(if n == 0 { 0.0 } else { kh.total() / n as f64 }), b'D')?),
        (kind::MAPPING | kind::FILTERING | kind::FLAT_MAPPING, a) => acc_finish(vm, args[1].r(), a)?,
        (kind::AND_THEN, a) => {
            let r = acc_finish(vm, args[0].r(), a)?;
            vm.invoke_virtual(args[1].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[r])?
        }
        (kind::TEEING, Acc::Pair(x, y)) => {
            let p = acc_finish(vm, args[0].r(), *x)?;
            let q = acc_finish(vm, args[1].r(), *y)?;
            vm.invoke_virtual(args[2].r(), "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[p, q])?
        }
        (kind::MIN_BY | kind::MAX_BY, Acc::Best(b)) => util::optional_of(vm, b.unwrap_or(Value::NULL))?,
        (kind::REDUCING, Acc::Reduce(r)) => {
            if args[0].i() != 0 {
                r.unwrap_or(args[1])
            } else {
                util::optional_of(vm, r.unwrap_or(Value::NULL))?
            }
        }
        (kind::OF, Acc::Custom(container, _)) => {
            if args[3].r() != 0 {
                vm.invoke_virtual(args[3].r(), "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[container])?
            } else {
                container
            }
        }
        (_, Acc::Container(v)) => v,
        (_, Acc::Items(items)) => Value::Ref(new_list(vm, items)?),
        (k, a) => return Err(Exc::Internal(format!("finish collector {k} with {a:?}"))),
    })
}

/// Whether a program's collector declares `IDENTITY_FINISH`.
fn custom_identity_finish(vm: &mut Vm, c: ObjId) -> Res<bool> {
    let set = vm.invoke_virtual(c, "characteristics", "()Ljava/util/Set;", &[])?.r();
    if set == 0 {
        return Ok(false);
    }
    let id = vm.get_static("java/util/stream/Collector$Characteristics", "IDENTITY_FINISH", "Ljava/util/stream/Collector$Characteristics;")?;
    Ok(vm.invoke_virtual(set, "contains", "(Ljava/lang/Object;)Z", &[id])?.z())
}

fn key_fn(kind: u8) -> (&'static str, &'static str) {
    match kind {
        b'I' => ("applyAsInt", "(Ljava/lang/Object;)I"),
        b'J' => ("applyAsLong", "(Ljava/lang/Object;)J"),
        _ => ("applyAsDouble", "(Ljava/lang/Object;)D"),
    }
}

fn text(vm: &mut Vm, v: Value) -> Res<String> {
    Ok(String::from_utf16_lossy(&vm.stringify(v, "Ljava/lang/Object;")?))
}

/// `get` on a native map, or through `Map.get` on a program's own.
fn map_get_any(vm: &mut Vm, m: ObjId, key: Value) -> Res<Option<Value>> {
    if matches!(state(vm, m), Some(State::Map(_) | State::Tree(_))) {
        return map_get(vm, m, key);
    }
    let v = vm.invoke_virtual(m, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[key])?;
    Ok((v.r() != 0).then_some(v))
}

fn map_put_any(vm: &mut Vm, m: ObjId, key: Value, value: Value) -> Res<()> {
    if matches!(state(vm, m), Some(State::Map(_) | State::Tree(_))) {
        map_put(vm, m, key, value)?;
        return Ok(());
    }
    let boxed = match value {
        Value::Int(i) => Value::Ref(super::lang::box_value(vm, Value::Int(i), b'I')?),
        v => v,
    };
    vm.invoke_virtual(m, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[key, boxed])?;
    Ok(())
}

fn collectors(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let empty = vm.intern("")?;
    let to_list = |vm: &mut Vm| -> Res<ObjId> { new_collector(vm, kind::TO_LIST, &[]).map(|v| v.r()) };
    match op {
        "toList" => new_collector(vm, kind::TO_LIST, &[]),
        "toSet" => new_collector(vm, kind::TO_SET, &[]),
        "toUnmodifiableList" => new_collector(vm, kind::UNMODIFIABLE_LIST, &[]),
        "toUnmodifiableSet" => new_collector(vm, kind::UNMODIFIABLE_SET, &[]),
        "toMap" => new_collector(vm, kind::TO_MAP, a),
        "toUnmodifiableMap" => new_collector(vm, kind::UNMODIFIABLE_MAP, a),
        "toCollection" => new_collector(vm, kind::TO_COLLECTION, a),
        "joining" => {
            let d = if a.is_empty() { Value::Ref(empty) } else { arg(a, 0) };
            let (p, s) = if a.len() == 3 { (arg(a, 1), arg(a, 2)) } else { (Value::Ref(empty), Value::Ref(empty)) };
            if d.r() == 0 || p.r() == 0 || s.r() == 0 {
                return Err(vm.npe(None));
            }
            new_collector(vm, kind::JOINING, &[d, p, s])
        }
        "groupingBy1" => {
            let down = to_list(vm)?;
            new_collector(vm, kind::GROUPING, &[arg(a, 0), Value::NULL, Value::Ref(down)])
        }
        "groupingBy2" => new_collector(vm, kind::GROUPING, &[arg(a, 0), Value::NULL, arg(a, 1)]),
        "groupingBy3" => new_collector(vm, kind::GROUPING, &[arg(a, 0), arg(a, 1), arg(a, 2)]),
        "partitioningBy1" => {
            let down = to_list(vm)?;
            new_collector(vm, kind::PARTITIONING, &[arg(a, 0), Value::Ref(down)])
        }
        "partitioningBy2" => new_collector(vm, kind::PARTITIONING, &[arg(a, 0), arg(a, 1)]),
        "counting" => new_collector(vm, kind::COUNTING, &[]),
        "summingInt" => new_collector(vm, kind::SUMMING, &[arg(a, 0), Value::Int(b'I' as i32)]),
        "summingLong" => new_collector(vm, kind::SUMMING, &[arg(a, 0), Value::Int(b'J' as i32)]),
        "summingDouble" => new_collector(vm, kind::SUMMING, &[arg(a, 0), Value::Int(b'D' as i32)]),
        "averagingInt" => new_collector(vm, kind::AVERAGING, &[arg(a, 0), Value::Int(b'I' as i32)]),
        "averagingLong" => new_collector(vm, kind::AVERAGING, &[arg(a, 0), Value::Int(b'J' as i32)]),
        "averagingDouble" => new_collector(vm, kind::AVERAGING, &[arg(a, 0), Value::Int(b'D' as i32)]),
        "summarizingInt" => new_collector(vm, kind::SUMMARIZING, &[arg(a, 0), Value::Int(b'I' as i32)]),
        "summarizingLong" => new_collector(vm, kind::SUMMARIZING, &[arg(a, 0), Value::Int(b'J' as i32)]),
        "summarizingDouble" => new_collector(vm, kind::SUMMARIZING, &[arg(a, 0), Value::Int(b'D' as i32)]),
        "mapping" => new_collector(vm, kind::MAPPING, a),
        "filtering" => new_collector(vm, kind::FILTERING, a),
        "flatMapping" => new_collector(vm, kind::FLAT_MAPPING, a),
        "collectingAndThen" => new_collector(vm, kind::AND_THEN, a),
        "teeing" => new_collector(vm, kind::TEEING, a),
        "minBy" => new_collector(vm, kind::MIN_BY, a),
        "maxBy" => new_collector(vm, kind::MAX_BY, a),
        "reducing1" => new_collector(vm, kind::REDUCING, &[Value::Int(0), Value::NULL, Value::NULL, arg(a, 0)]),
        "reducing2" => new_collector(vm, kind::REDUCING, &[Value::Int(1), arg(a, 0), Value::NULL, arg(a, 1)]),
        "reducing3" => new_collector(vm, kind::REDUCING, &[Value::Int(1), arg(a, 0), arg(a, 1), arg(a, 2)]),
        "of4" => new_collector(vm, kind::OF, &[arg(a, 0), arg(a, 1), arg(a, 2), Value::NULL]),
        "of5" => new_collector(vm, kind::OF, &[arg(a, 0), arg(a, 1), arg(a, 2), arg(a, 3)]),
        _ => Err(Exc::Unsupported(format!("Collectors.{op}"))),
    }
}

// ------------------------------------------------------------------ classes

macro_rules! pipeline_methods {
    ($s:literal, $fn_map:literal, $pred:literal, $cons:literal, $bin:literal, $opt:literal, $p:literal, $arr:literal) => {
        natives![
            "filter" (concat!("(L", $pred, ";)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "filter")),
            "map" (concat!("(L", $fn_map, ";)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "map")),
            "peek" (concat!("(L", $cons, ";)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "peek")),
            "limit" (concat!("(J)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "limit")),
            "skip" (concat!("(J)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "skip")),
            "distinct" (concat!("()L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "distinct")),
            "sorted" (concat!("()L", $s, ";")) M => Some(|vm, a| intermediate(vm, &a[..1], "sorted")),
            "takeWhile" (concat!("(L", $pred, ";)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "takeWhile")),
            "dropWhile" (concat!("(L", $pred, ";)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "dropWhile")),
            "sequential" (concat!("()L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "sequential")),
            "parallel" (concat!("()L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "parallel")),
            "forEach" (concat!("(L", $cons, ";)V")) M => Some(|vm, a| terminal(vm, a, "forEach")),
            "forEachOrdered" (concat!("(L", $cons, ";)V")) M => Some(|vm, a| terminal(vm, a, "forEachOrdered")),
            "count" "()J" M => Some(|vm, a| terminal(vm, a, "count")),
            "reduce" (concat!("(L", $bin, ";)L", $opt, ";")) M => Some(|vm, a| terminal(vm, a, "reduce")),
            "reduce" (concat!("(", $p, "L", $bin, ";)", $p)) M => Some(|vm, a| terminal(vm, a, "reduceIdentity")),
            "anyMatch" (concat!("(L", $pred, ";)Z")) M => Some(|vm, a| terminal(vm, a, "anyMatch")),
            "allMatch" (concat!("(L", $pred, ";)Z")) M => Some(|vm, a| terminal(vm, a, "allMatch")),
            "noneMatch" (concat!("(L", $pred, ";)Z")) M => Some(|vm, a| terminal(vm, a, "noneMatch")),
            "findFirst" (concat!("()L", $opt, ";")) M => Some(|vm, a| terminal(vm, a, "findFirst")),
            "findAny" (concat!("()L", $opt, ";")) M => Some(|vm, a| terminal(vm, a, "findAny")),
            "toArray" (concat!("()", $arr)) M => Some(|vm, a| terminal(vm, a, "toArray")),
            "iterator" "()Ljava/util/Iterator;" M => Some(|vm, a| terminal(vm, a, "iterator")),
            "close" "()V" M => Some(|vm, a| terminal(vm, a, "close")),
            "isParallel" "()Z" M => Some(|vm, a| terminal(vm, a, "isParallel")),
            "unordered" "()Ljava/util/stream/BaseStream;" M => Some(|vm, a| intermediate(vm, a, "unordered")),
            "onClose" "(Ljava/lang/Runnable;)Ljava/util/stream/BaseStream;" M => Some(|vm, a| intermediate(vm, a, "onClose")),
        ]
    };
}

static REFERENCE_METHODS: &[NativeMethod] = pipeline_methods!(
    "java/util/stream/Stream",
    "java/util/function/Function",
    "java/util/function/Predicate",
    "java/util/function/Consumer",
    "java/util/function/BinaryOperator",
    "java/util/Optional",
    "Ljava/lang/Object;",
    "[Ljava/lang/Object;"
);

static REFERENCE_EXTRA: &[NativeMethod] = natives![
    "sorted" "(Ljava/util/Comparator;)Ljava/util/stream/Stream;" M => Some(|vm, a| intermediate(vm, a, "sorted")),
    "flatMap" "(Ljava/util/function/Function;)Ljava/util/stream/Stream;" M => Some(|vm, a| intermediate(vm, a, "flatMap")),
    "flatMapToInt" "(Ljava/util/function/Function;)Ljava/util/stream/IntStream;" M => Some(|vm, a| intermediate(vm, a, "flatMapToInt")),
    "mapToInt" "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/IntStream;" M => Some(|vm, a| intermediate(vm, a, "mapToInt")),
    "mapToLong" "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/LongStream;" M => Some(|vm, a| intermediate(vm, a, "mapToLong")),
    "mapToDouble" "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/DoubleStream;" M => Some(|vm, a| intermediate(vm, a, "mapToDouble")),
    "reduce" "(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;" M => Some(|vm, a| terminal(vm, a, "reduceCombiner")),
    "collect" "(Ljava/util/stream/Collector;)Ljava/lang/Object;" M => Some(|vm, a| terminal(vm, a, "collect")),
    "collect" "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;" M => Some(|vm, a| terminal(vm, a, "collect3")),
    "toList" "()Ljava/util/List;" M => Some(|vm, a| terminal(vm, a, "toList")),
    "toArray" "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;" M => Some(|vm, a| terminal(vm, a, "toArrayTyped")),
    "min" "(Ljava/util/Comparator;)Ljava/util/Optional;" M => Some(|vm, a| terminal(vm, a, "min")),
    "max" "(Ljava/util/Comparator;)Ljava/util/Optional;" M => Some(|vm, a| terminal(vm, a, "max")),
];

macro_rules! primitive_extra {
    ($s:literal, $p:literal, $opt:literal, $obj_fn:literal, $sup:literal, $objcons:literal) => {
        natives![
            "mapToObj" (concat!("(L", $obj_fn, ";)Ljava/util/stream/Stream;")) M => Some(|vm, a| intermediate(vm, a, "mapToObj")),
            "flatMap" (concat!("(L", $obj_fn, ";)L", $s, ";")) M => Some(|vm, a| intermediate(vm, a, "flatMap")),
            "boxed" "()Ljava/util/stream/Stream;" M => Some(|vm, a| intermediate(vm, a, "boxed")),
            "sum" (concat!("()", $p)) M => Some(|vm, a| terminal(vm, a, "sum")),
            "average" "()Ljava/util/OptionalDouble;" M => Some(|vm, a| terminal(vm, a, "average")),
            "min" (concat!("()L", $opt, ";")) M => Some(|vm, a| terminal(vm, &a[..1], "min")),
            "max" (concat!("()L", $opt, ";")) M => Some(|vm, a| terminal(vm, &a[..1], "max")),
            "collect" (concat!("(Ljava/util/function/Supplier;L", $objcons, ";Ljava/util/function/BiConsumer;)Ljava/lang/Object;")) M => Some(|vm, a| terminal(vm, a, "collect3")),
            "asDoubleStream" "()Ljava/util/stream/DoubleStream;" M => Some(|vm, a| intermediate(vm, a, "asDoubleStream")),
        ]
    };
}

static INT_METHODS: &[NativeMethod] = pipeline_methods!(
    "java/util/stream/IntStream",
    "java/util/function/IntUnaryOperator",
    "java/util/function/IntPredicate",
    "java/util/function/IntConsumer",
    "java/util/function/IntBinaryOperator",
    "java/util/OptionalInt",
    "I",
    "[I"
);
static INT_EXTRA: &[NativeMethod] = primitive_extra!("java/util/stream/IntStream", "I", "java/util/OptionalInt", "java/util/function/IntFunction", "java/util/function/IntSupplier", "java/util/function/ObjIntConsumer");
static INT_EXTRA2: &[NativeMethod] = natives![
    "asLongStream" "()Ljava/util/stream/LongStream;" M => Some(|vm, a| intermediate(vm, a, "asLongStream")),
    "mapToLong" "(Ljava/util/function/IntToLongFunction;)Ljava/util/stream/LongStream;" M => Some(|vm, a| intermediate(vm, a, "mapToLong")),
    "mapToDouble" "(Ljava/util/function/IntToDoubleFunction;)Ljava/util/stream/DoubleStream;" M => Some(|vm, a| intermediate(vm, a, "mapToDouble")),
    "summaryStatistics" "()Ljava/util/IntSummaryStatistics;" M => Some(|vm, a| terminal(vm, a, "summaryStatistics")),
];

static LONG_METHODS: &[NativeMethod] = pipeline_methods!(
    "java/util/stream/LongStream",
    "java/util/function/LongUnaryOperator",
    "java/util/function/LongPredicate",
    "java/util/function/LongConsumer",
    "java/util/function/LongBinaryOperator",
    "java/util/OptionalLong",
    "J",
    "[J"
);
static LONG_EXTRA: &[NativeMethod] = primitive_extra!("java/util/stream/LongStream", "J", "java/util/OptionalLong", "java/util/function/LongFunction", "java/util/function/LongSupplier", "java/util/function/ObjLongConsumer");
static LONG_EXTRA2: &[NativeMethod] = natives![
    "mapToInt" "(Ljava/util/function/LongToIntFunction;)Ljava/util/stream/IntStream;" M => Some(|vm, a| intermediate(vm, a, "mapToInt")),
    "summaryStatistics" "()Ljava/util/LongSummaryStatistics;" M => Some(|vm, a| terminal(vm, a, "summaryStatistics")),
];

static DOUBLE_METHODS: &[NativeMethod] = pipeline_methods!(
    "java/util/stream/DoubleStream",
    "java/util/function/DoubleUnaryOperator",
    "java/util/function/DoublePredicate",
    "java/util/function/DoubleConsumer",
    "java/util/function/DoubleBinaryOperator",
    "java/util/OptionalDouble",
    "D",
    "[D"
);
static DOUBLE_EXTRA: &[NativeMethod] = primitive_extra!("java/util/stream/DoubleStream", "D", "java/util/OptionalDouble", "java/util/function/DoubleFunction", "java/util/function/DoubleSupplier", "java/util/function/ObjDoubleConsumer");
static DOUBLE_EXTRA2: &[NativeMethod] = natives![
    "mapToInt" "(Ljava/util/function/DoubleToIntFunction;)Ljava/util/stream/IntStream;" M => Some(|vm, a| intermediate(vm, a, "mapToInt")),
    "summaryStatistics" "()Ljava/util/DoubleSummaryStatistics;" M => Some(|vm, a| terminal(vm, a, "summaryStatistics")),
];

macro_rules! stats_class {
    ($name:literal, $p:literal, $kind:expr) => {
        NativeClass {
            name: $name,
            super_name: Some("java/lang/Object"),
            interfaces: &[],
            access: PUBLIC,
            methods: natives![
                "<init>" "()V" M => Some(|vm, a| { let t = this(vm, a)?; stats_init(vm, t, $kind); void() }),
                "accept" (concat!("(", $p, ")V")) M => Some(|vm, a| stats_op(vm, a, "accept")),
                "accept" "(I)V" M => Some(|vm, a| stats_op(vm, a, "accept")),
                "combine" (concat!("(L", $name, ";)V")) M => Some(|vm, a| stats_op(vm, a, "combine")),
                "getCount" "()J" M => Some(|vm, a| stats_op(vm, a, "getCount")),
                "getSum" (concat!("()", $p)) M => Some(|vm, a| stats_op(vm, a, "getSum")),
                "getMin" (concat!("()", $p)) M => Some(|vm, a| stats_op(vm, a, "getMin")),
                "getMax" (concat!("()", $p)) M => Some(|vm, a| stats_op(vm, a, "getMax")),
                "getAverage" "()D" M => Some(|vm, a| stats_op(vm, a, "getAverage")),
                "toString" "()Ljava/lang/String;" M => Some(|vm, a| stats_op(vm, a, "toString")),
            ],
            statics: &[],
            clinit: None,
            data: Some(|| Data::Jdk(Box::new(State::Generic(Vec::new())))),
        }
    };
}

static CLASSES: &[NativeClass] = &[
    class("java/util/stream/BaseStream", None, &["java/lang/AutoCloseable"], INTERFACE, natives!["iterator" "()Ljava/util/Iterator;" A => None]),
    class("java/util/stream/Stream", None, &["java/util/stream/BaseStream"], INTERFACE, natives![
        "of" "(Ljava/lang/Object;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "of1", b'L')),
        "of" "([Ljava/lang/Object;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "ofArray", b'L')),
        "ofNullable" "(Ljava/lang/Object;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "ofNullable", b'L')),
        "empty" "()Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "empty", b'L')),
        "iterate" "(Ljava/lang/Object;Ljava/util/function/UnaryOperator;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "iterate", b'L')),
        "iterate" "(Ljava/lang/Object;Ljava/util/function/Predicate;Ljava/util/function/UnaryOperator;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "iterate3", b'L')),
        "generate" "(Ljava/util/function/Supplier;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "generate", b'L')),
        "concat" "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;" S => Some(|vm, a| statics(vm, a, "concat", b'L')),
        "builder" "()Ljava/util/stream/Stream$Builder;" S => Some(|vm, _| { let c = vm.load("java/util/stream/Stream$Builder$Native")?; let r = vm.new_instance(c)?; if let Some(o) = vm.obj_mut(r) { o.data = Data::Jdk(Box::new(State::List(util::JList::default()))); } ret_ref(r) }),
    ]),
    class("java/util/stream/IntStream", None, &["java/util/stream/BaseStream"], INTERFACE, natives![
        "of" "(I)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "of1", b'I')),
        "of" "([I)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "ofArray", b'I')),
        "empty" "()Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "empty", b'I')),
        "range" "(II)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "range", b'I')),
        "rangeClosed" "(II)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "rangeClosed", b'I')),
        "iterate" "(ILjava/util/function/IntUnaryOperator;)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "iterate", b'I')),
        "iterate" "(ILjava/util/function/IntPredicate;Ljava/util/function/IntUnaryOperator;)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "iterate3", b'I')),
        "generate" "(Ljava/util/function/IntSupplier;)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "generate", b'I')),
        "concat" "(Ljava/util/stream/IntStream;Ljava/util/stream/IntStream;)Ljava/util/stream/IntStream;" S => Some(|vm, a| statics(vm, a, "concat", b'I')),
    ]),
    class("java/util/stream/LongStream", None, &["java/util/stream/BaseStream"], INTERFACE, natives![
        "of" "(J)Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "of1", b'J')),
        "of" "([J)Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "ofArray", b'J')),
        "empty" "()Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "empty", b'J')),
        "range" "(JJ)Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "range", b'J')),
        "rangeClosed" "(JJ)Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "rangeClosed", b'J')),
        "iterate" "(JLjava/util/function/LongUnaryOperator;)Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "iterate", b'J')),
        "generate" "(Ljava/util/function/LongSupplier;)Ljava/util/stream/LongStream;" S => Some(|vm, a| statics(vm, a, "generate", b'J')),
    ]),
    class("java/util/stream/DoubleStream", None, &["java/util/stream/BaseStream"], INTERFACE, natives![
        "of" "(D)Ljava/util/stream/DoubleStream;" S => Some(|vm, a| statics(vm, a, "of1", b'D')),
        "of" "([D)Ljava/util/stream/DoubleStream;" S => Some(|vm, a| statics(vm, a, "ofArray", b'D')),
        "empty" "()Ljava/util/stream/DoubleStream;" S => Some(|vm, a| statics(vm, a, "empty", b'D')),
        "iterate" "(DLjava/util/function/DoubleUnaryOperator;)Ljava/util/stream/DoubleStream;" S => Some(|vm, a| statics(vm, a, "iterate", b'D')),
        "generate" "(Ljava/util/function/DoubleSupplier;)Ljava/util/stream/DoubleStream;" S => Some(|vm, a| statics(vm, a, "generate", b'D')),
    ]),
    NativeClass { name: "java/util/stream/ReferencePipeline", super_name: Some("java/lang/Object"), interfaces: &["java/util/stream/Stream"], access: PUBLIC | FINAL, methods: REFERENCE_METHODS, statics: &[], clinit: None, data: None },
    class("java/util/stream/ReferencePipeline$Extra", None, &[], 0, REFERENCE_EXTRA),
    NativeClass { name: "java/util/stream/IntPipeline", super_name: Some("java/lang/Object"), interfaces: &["java/util/stream/IntStream"], access: PUBLIC | FINAL, methods: INT_METHODS, statics: &[], clinit: None, data: None },
    class("java/util/stream/IntPipeline$Extra", None, &[], 0, INT_EXTRA),
    class("java/util/stream/IntPipeline$Extra2", None, &[], 0, INT_EXTRA2),
    NativeClass { name: "java/util/stream/LongPipeline", super_name: Some("java/lang/Object"), interfaces: &["java/util/stream/LongStream"], access: PUBLIC | FINAL, methods: LONG_METHODS, statics: &[], clinit: None, data: None },
    class("java/util/stream/LongPipeline$Extra", None, &[], 0, LONG_EXTRA),
    class("java/util/stream/LongPipeline$Extra2", None, &[], 0, LONG_EXTRA2),
    NativeClass { name: "java/util/stream/DoublePipeline", super_name: Some("java/lang/Object"), interfaces: &["java/util/stream/DoubleStream"], access: PUBLIC | FINAL, methods: DOUBLE_METHODS, statics: &[], clinit: None, data: None },
    class("java/util/stream/DoublePipeline$Extra", None, &[], 0, DOUBLE_EXTRA),
    class("java/util/stream/DoublePipeline$Extra2", None, &[], 0, DOUBLE_EXTRA2),
    NativeClass { name: "java/util/stream/StreamIterator", super_name: Some("java/lang/Object"), interfaces: &["java/util/Iterator"], access: FINAL, methods: natives![
        "hasNext" "()Z" M => Some(|vm, a| iterator_op(vm, a, "hasNext")),
        "next" "()Ljava/lang/Object;" M => Some(|vm, a| iterator_op(vm, a, "next")),
        "nextInt" "()I" M => Some(|vm, a| iterator_op(vm, a, "nextPrimitive")),
    ], statics: &[], clinit: None, data: None },
    class("java/util/stream/Stream$Builder", None, &["java/util/function/Consumer"], INTERFACE, natives!["build" "()Ljava/util/stream/Stream;" A => None]),
    NativeClass { name: "java/util/stream/Stream$Builder$Native", super_name: Some("java/lang/Object"), interfaces: &["java/util/stream/Stream$Builder"], access: FINAL, methods: natives![
        "accept" "(Ljava/lang/Object;)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(State::List(l)) = super::state_mut(vm, t) { l.items.push(arg(a, 1)); } void() }),
        "add" "(Ljava/lang/Object;)Ljava/util/stream/Stream$Builder;" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(State::List(l)) = super::state_mut(vm, t) { l.items.push(arg(a, 1)); } ret_ref(t) }),
        "build" "()Ljava/util/stream/Stream;" M => Some(|vm, a| { let t = this(vm, a)?; let items = match state(vm, t) { Some(State::List(l)) => l.items.clone(), _ => Vec::new() }; stream_of(vm, items) }),
    ], statics: &[], clinit: None, data: None },
    class("java/util/stream/Collector", None, &[], INTERFACE, natives![
        "supplier" "()Ljava/util/function/Supplier;" A => None,
        "accumulator" "()Ljava/util/function/BiConsumer;" A => None,
        "of" "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BinaryOperator;[Ljava/util/stream/Collector$Characteristics;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "of4")),
        "of" "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BinaryOperator;Ljava/util/function/Function;[Ljava/util/stream/Collector$Characteristics;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "of5")),
    ]),
    NativeClass {
        name: "java/util/stream/Collector$Characteristics",
        super_name: Some("java/lang/Enum"),
        interfaces: &[],
        access: PUBLIC | FINAL | 0x4000,
        methods: natives!["values" "()[Ljava/util/stream/Collector$Characteristics;" S => Some(|vm, _| super::math::enum_values(vm, "java/util/stream/Collector$Characteristics"))],
        statics: &[
            ("CONCURRENT", "Ljava/util/stream/Collector$Characteristics;"),
            ("UNORDERED", "Ljava/util/stream/Collector$Characteristics;"),
            ("IDENTITY_FINISH", "Ljava/util/stream/Collector$Characteristics;"),
            ("$VALUES", "[Ljava/util/stream/Collector$Characteristics;"),
        ],
        clinit: Some(|vm, _| super::math::make_enum_constants(vm, "java/util/stream/Collector$Characteristics", &["CONCURRENT", "UNORDERED", "IDENTITY_FINISH"])),
        data: None,
    },
    NativeClass { name: "java/util/stream/Collectors$CollectorImpl", super_name: Some("java/lang/Object"), interfaces: &["java/util/stream/Collector"], access: FINAL, methods: natives![
        "supplier" "()Ljava/util/function/Supplier;" M => Some(|_, _| Err(Exc::Unsupported("Collector.supplier of a JDK collector".into()))),
        "accumulator" "()Ljava/util/function/BiConsumer;" M => Some(|_, _| Err(Exc::Unsupported("Collector.accumulator of a JDK collector".into()))),
    ], statics: &[], clinit: None, data: None },
    class("java/util/stream/Collectors", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![
        "toList" "()Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toList")),
        "toSet" "()Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toSet")),
        "toUnmodifiableList" "()Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toUnmodifiableList")),
        "toUnmodifiableSet" "()Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toUnmodifiableSet")),
        "toMap" "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toMap")),
        "toMap" "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toMap")),
        "toMap" "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;Ljava/util/function/Supplier;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toMap")),
        "toUnmodifiableMap" "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toUnmodifiableMap")),
        "toCollection" "(Ljava/util/function/Supplier;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "toCollection")),
        "joining" "()Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "joining")),
        "joining" "(Ljava/lang/CharSequence;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "joining")),
        "joining" "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "joining")),
        "groupingBy" "(Ljava/util/function/Function;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "groupingBy1")),
        "groupingBy" "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "groupingBy2")),
        "groupingBy" "(Ljava/util/function/Function;Ljava/util/function/Supplier;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "groupingBy3")),
        "partitioningBy" "(Ljava/util/function/Predicate;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "partitioningBy1")),
        "partitioningBy" "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "partitioningBy2")),
        "counting" "()Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "counting")),
        "summingInt" "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "summingInt")),
        "summingLong" "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "summingLong")),
        "summingDouble" "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "summingDouble")),
        "averagingInt" "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "averagingInt")),
        "averagingLong" "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "averagingLong")),
        "averagingDouble" "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "averagingDouble")),
        "summarizingInt" "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "summarizingInt")),
        "summarizingLong" "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "summarizingLong")),
        "summarizingDouble" "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "summarizingDouble")),
        "mapping" "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "mapping")),
        "filtering" "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "filtering")),
        "flatMapping" "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "flatMapping")),
        "collectingAndThen" "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "collectingAndThen")),
        "teeing" "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "teeing")),
        "minBy" "(Ljava/util/Comparator;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "minBy")),
        "maxBy" "(Ljava/util/Comparator;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "maxBy")),
        "reducing" "(Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "reducing1")),
        "reducing" "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "reducing2")),
        "reducing" "(Ljava/lang/Object;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;" S => Some(|vm, a| collectors(vm, a, "reducing3")),
    ]),
    stats_class!("java/util/IntSummaryStatistics", "I", b'I'),
    stats_class!("java/util/LongSummaryStatistics", "J", b'J'),
    stats_class!("java/util/DoubleSummaryStatistics", "D", b'D'),
    class("java/util/stream/StreamSupport", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compensated_sum_matches_java() {
        // DoubleStream.of(0.1, 0.2, 0.3).sum() on HotSpot: 0.6 (a naive sum gives 0.6000000000000001).
        let mut k = Kahan::default();
        for v in [0.1, 0.2, 0.3] {
            k.add(v);
        }
        assert_eq!(k.total(), 0.6);
    }
}

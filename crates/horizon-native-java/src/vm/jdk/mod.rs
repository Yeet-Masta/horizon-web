//! The JDK, natively (docs/java-interpreter.md, decision 1): each class is a
//! static description whose methods are Rust functions over the VM. Bytecode
//! may extend these classes and implement these interfaces; a native method
//! calls back into bytecode through `Vm::invoke` where Java would.
use super::interp::Vm;
use super::{ClassId, Data, Exc, ObjId, Res, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

pub mod boxes;
pub mod invoke;
pub mod io;
pub mod lang;
pub mod math;
pub mod misc;
pub mod reflect;
pub mod stream;
pub mod string;
pub mod thread;
pub mod util;

pub type NativeFn = fn(&mut Vm, &[Value]) -> Res<Value>;

#[derive(Debug)]
pub struct NativeMethod {
    pub name: &'static str,
    pub desc: &'static str,
    pub access: u16,
    /// `None`: abstract (an interface method bytecode implements).
    pub f: Option<NativeFn>,
}

#[derive(Debug)]
pub struct NativeClass {
    pub name: &'static str,
    pub super_name: Option<&'static str>,
    pub interfaces: &'static [&'static str],
    pub access: u16,
    pub methods: &'static [NativeMethod],
    /// Static fields, set by `clinit`.
    pub statics: &'static [(&'static str, &'static str)],
    pub clinit: Option<NativeFn>,
    /// The native state a new instance (of this class or a subclass) starts with.
    pub data: Option<fn() -> Data>,
}

pub const PUBLIC: u16 = 0x0001;
pub const STATIC: u16 = 0x0008;
pub const FINAL: u16 = 0x0010;
pub const INTERFACE: u16 = 0x0200 | 0x0400;
pub const ABSTRACT: u16 = 0x0400;
/// A JDK method Java declares `native` (reflection shows it).
pub const NATIVE: u16 = 0x0100;
/// Access of a public instance method.
pub const M: u16 = PUBLIC;
/// Access of a public static method.
pub const S: u16 = PUBLIC | STATIC;
/// Access of an abstract interface method.
pub const A: u16 = PUBLIC | ABSTRACT;

/// Describes native methods: `"name" "desc" access => function`.
#[macro_export]
macro_rules! natives {
    ($( $name:tt $desc:tt $acc:expr => $f:expr ),* $(,)?) => {
        &[ $( $crate::vm::jdk::NativeMethod { name: $name, desc: $desc, access: $acc, f: $f } ),* ]
    };
}

/// A class with no members of its own (a marker interface, an exception
/// whose constructors come from [`lang::THROWABLE_CTORS`]...).
pub const fn class(
    name: &'static str,
    super_name: Option<&'static str>,
    interfaces: &'static [&'static str],
    access: u16,
    methods: &'static [NativeMethod],
) -> NativeClass {
    NativeClass {
        name,
        super_name,
        interfaces,
        access,
        methods,
        statics: &[],
        clinit: None,
        data: None,
    }
}

fn registry() -> &'static HashMap<&'static str, &'static NativeClass> {
    static REGISTRY: OnceLock<HashMap<&'static str, &'static NativeClass>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut map = HashMap::new();
        for group in [
            lang::classes(),
            string::classes(),
            boxes::classes(),
            util::classes(),
            io::classes(),
            invoke::classes(),
            reflect::classes(),
            thread::classes(),
            stream::classes(),
            math::classes(),
            misc::classes(),
        ] {
            for c in group {
                map.insert(c.name, c);
            }
        }
        map
    })
}

/// The native description of a JDK class, if the VM has one.
pub fn find(name: &str) -> Option<&'static NativeClass> {
    registry().get(name).copied()
}

/// Whether a name is the JDK's to define: one the VM lacks is unsupported,
/// not a class the program could have shipped.
pub fn is_jdk_name(name: &str) -> bool {
    ["java/", "javax/", "jdk/", "sun/", "com/sun/"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// A `native` method a class file declares: none are linked (JNI is outside
/// the sandbox), so calling one throws `UnsatisfiedLinkError`.
pub fn bytecode_class_native(_class: &str, _name: &str, _desc: &str) -> Option<NativeFn> {
    None
}

/// The native state a new instance of `class` starts with.
pub fn initial_data(vm: &Vm, class: ClassId) -> Data {
    let mut c = Some(class);
    while let Some(id) = c {
        let cls = vm.class(id);
        if let super::ClassKind::Native(def) = cls.kind
            && let Some(make) = def.data
        {
            return make();
        }
        c = cls.super_class;
    }
    Data::None
}

/// The rest of the native library's per-object state.
#[derive(Debug, Clone)]
pub enum State {
    Enum { name: ObjId, ordinal: i32 },
    /// `ArrayList`, `LinkedList`, `ArrayDeque`, immutable lists, `Vector`...
    List(util::JList),
    Map(Box<util::JMap>),
    Tree(Box<util::JTree>),
    Heap(Box<util::JHeap>),
    /// An iterator over a snapshot or a live collection.
    Iter(Box<util::JIter>),
    /// A `Map.Entry` of a map, or a free-standing one.
    Entry { key: Value, value: Value, map: ObjId },
    Optional(Value, bool),
    Random(u64, Option<f64>),
    Atomic(Value),
    Stream(Box<stream::JStream>),
    Out(Box<io::Out>),
    In(Box<io::Source>),
    Thread(Box<thread::JThread>),
    Reflect(Box<reflect::Reflected>),
    Handle(Box<invoke::Handle>),
    Joiner(Box<util::Joiner>),
    Big(Box<math::Big>),
    Regex(Box<util::Regex>),
    Bytes(Vec<u8>, usize),
    Generic(Vec<Value>),
}

impl State {
    pub fn refs(&self, out: &mut Vec<ObjId>) {
        let mut push = |v: &Value| {
            if let Value::Ref(r) = v {
                out.push(*r);
            }
        };
        match self {
            State::Enum { name, .. } => push(&Value::Ref(*name)),
            State::List(l) => l.items.iter().for_each(&mut push),
            State::Map(m) => m.values().for_each(|(k, v)| {
                push(&k);
                push(&v);
            }),
            State::Tree(t) => {
                t.entries.iter().for_each(|(k, v)| {
                    push(k);
                    push(v);
                });
                push(&Value::Ref(t.comparator));
            }
            State::Heap(h) => {
                h.items.iter().for_each(&mut push);
                push(&Value::Ref(h.comparator));
            }
            State::Iter(i) => i.refs(&mut |v| push(&v)),
            State::Entry { key, value, map } => {
                push(key);
                push(value);
                push(&Value::Ref(*map));
            }
            State::Optional(v, _) | State::Atomic(v) => push(v),
            State::Stream(s) => s.refs(&mut |v| push(&v)),
            State::Thread(t) => t.refs(&mut |v| push(&v)),
            State::Handle(h) => h.refs(&mut |v| push(&v)),
            State::Generic(items) => items.iter().for_each(&mut push),
            State::Out(o) => o.refs(&mut |r| push(&Value::Ref(r))),
            State::In(i) => i.refs(&mut |r| push(&Value::Ref(r))),
            _ => {}
        }
    }
}

/// The native library's per-VM state.
#[derive(Default)]
pub struct VmState {
    /// Held monitors: owner thread and recursion count.
    pub monitors: HashMap<ObjId, (usize, u32)>,
    pub record_descs: HashMap<(ClassId, usize), Arc<str>>,
    pub enum_descs: HashMap<ObjId, (ClassId, Arc<str>)>,
    pub condy_cache: HashMap<(usize, u16), Value>,
    pub next_lambda: u32,
    pub main_started: bool,
    /// The wrapper caches (`Integer.valueOf` and its kin), by kind and value.
    pub box_cache: HashMap<(u8, i64), ObjId>,
    pub properties: Vec<(String, String)>,
    pub threads: thread::Threads,
    /// Virtual time, in nanoseconds since the VM started.
    pub nanos: u64,
    pub seed_uniquifier: u64,
    pub polymorphic: HashMap<(ClassId, String, String), super::MethodId>,
    /// `ClassLoader.set*AssertionStatus`: by class name, by package (a
    /// trailing `.`), and the default.
    pub assertion_status: HashMap<String, bool>,
    /// `new Properties(defaults)`: each Properties' defaults.
    pub property_defaults: HashMap<ObjId, ObjId>,
    /// `ScopedValue` bindings in force, innermost last.
    pub scoped: Vec<(usize, ObjId, Value)>,
    pub default_assertions: Option<bool>,
    pub proxies: Vec<reflect::ProxyClass>,
}

// --------------------------------------------------------------- helpers

/// The `this` of an instance call, or a `NullPointerException`.
pub fn this(vm: &mut Vm, args: &[Value]) -> Res<ObjId> {
    match args.first() {
        Some(Value::Ref(r)) if *r != 0 => Ok(*r),
        _ => Err(vm.npe(None)),
    }
}

pub fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or_default()
}

pub fn ret_ref(r: ObjId) -> Res<Value> {
    Ok(Value::Ref(r))
}

pub fn ret_bool(b: bool) -> Res<Value> {
    Ok(Value::Int(b as i32))
}

pub fn ret_str(vm: &mut Vm, s: &str) -> Res<Value> {
    Ok(Value::Ref(vm.new_string(s)?))
}

pub fn void() -> Res<Value> {
    Ok(Value::Top)
}

/// The native state of an object, if it has the expected variant.
pub fn state(vm: &Vm, r: ObjId) -> Option<&State> {
    match &vm.obj(r)?.data {
        Data::Jdk(s) => Some(s),
        _ => None,
    }
}

pub fn state_mut(vm: &mut Vm, r: ObjId) -> Option<&mut State> {
    match &mut vm.obj_mut(r)?.data {
        Data::Jdk(s) => Some(s),
        _ => None,
    }
}

/// Takes an object's native state out while a native works on it with the
/// whole VM (calls back into bytecode included), and puts it back after.
pub fn with_state<T>(
    vm: &mut Vm,
    r: ObjId,
    f: impl FnOnce(&mut Vm, &mut State) -> Res<T>,
) -> Res<T> {
    let taken = match vm.obj_mut(r) {
        Some(o) => std::mem::take(&mut o.data),
        None => return Err(vm.npe(None)),
    };
    let Data::Jdk(mut s) = taken else {
        if let Some(o) = vm.obj_mut(r) {
            o.data = taken;
        }
        let name = vm.class_of(r).map(|c| vm.class(c).java_name()).unwrap_or_default();
        return Err(Exc::Internal(format!("{name} has no native state")));
    };
    let result = f(vm, &mut s);
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(s);
    }
    result
}

/// `IllegalArgumentException` and the like, with a message.
pub fn throw(vm: &mut Vm, class: &str, message: &str) -> Exc {
    vm.throw_new(class, Some(message))
}

//! A Java virtual machine for mods' own code, the libraries they bundle and
//! the parts of the JDK they use (docs/java-interpreter.md).
//!
//! Objects live on a heap of their own with identity, classes are loaded from
//! the mod's jars and linked as JVMS 5 says, each method is decoded once into
//! compact instructions whose references are resolved on first use, and the
//! core of the JDK is native Rust (`jdk/`). Nothing here reads a clock, a file,
//! the environment or a thread of the host: time is virtual, output is kept in
//! memory, and every limit (instructions, frames, heap) ends a run with an
//! error or the Java exception the JVM would throw.
//!
//! The VM is new beside the bounded interpreter in `lib.rs`, which keeps
//! running the game's entry points until they move here
//! (docs/java-interpreter.md, "The code, and moving onto it").

pub mod classfile;
mod class;
mod heap;
mod indy;
mod interp;
pub mod jdk;
mod text;

use std::collections::HashMap;
use std::sync::Arc;

pub use heap::{Array, Data, Obj};
pub use text::{java_double_to_string, java_float_to_string};

/// A reference: an index into the heap, 0 for `null`.
pub type ObjId = u32;
/// A loaded class.
pub type ClassId = u32;
/// A method of a loaded class.
pub type MethodId = u32;
/// An interned `(name, descriptor)` pair: what a call or a field is looked up by.
pub type NameDesc = u32;

/// One slot of a frame's locals or operand stack, or one field. A `long` or
/// `double` takes two slots in a frame (the value, then [`Value::Top`]), as the
/// JVM's own slots do, and one in a field.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Top,
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Ref(ObjId),
}

impl Value {
    pub const NULL: Value = Value::Ref(0);

    pub fn i(self) -> i32 {
        match self {
            Value::Int(v) => v,
            _ => 0,
        }
    }
    pub fn j(self) -> i64 {
        match self {
            Value::Long(v) => v,
            _ => 0,
        }
    }
    pub fn f(self) -> f32 {
        match self {
            Value::Float(v) => v,
            _ => 0.0,
        }
    }
    pub fn d(self) -> f64 {
        match self {
            Value::Double(v) => v,
            _ => 0.0,
        }
    }
    pub fn r(self) -> ObjId {
        match self {
            Value::Ref(v) => v,
            _ => 0,
        }
    }
    pub fn z(self) -> bool {
        self.i() != 0
    }
    pub fn is_wide(self) -> bool {
        matches!(self, Value::Long(_) | Value::Double(_))
    }
    /// The zero value a field of descriptor `desc` starts with.
    pub fn zero(desc: &str) -> Value {
        match desc.as_bytes().first() {
            Some(b'J') => Value::Long(0),
            Some(b'F') => Value::Float(0.0),
            Some(b'D') => Value::Double(0.0),
            Some(b'L' | b'[') => Value::NULL,
            _ => Value::Int(0),
        }
    }
}

/// Why a run stopped short of a normal return.
#[derive(Debug, Clone, PartialEq)]
pub enum Exc {
    /// A Java exception, propagating.
    Throw(ObjId),
    /// The program called `System.exit`.
    Exit(i32),
    /// Something the VM does not do yet (a JDK method no one wrote, a
    /// bootstrap it does not know): the run stops and names it.
    Unsupported(String),
    /// A limit of the sandbox: the instruction budget, nested native calls.
    Limit(String),
    /// A malformed class or an internal inconsistency: an error, not a panic.
    Internal(String),
}

pub type Res<T> = Result<T, Exc>;

pub fn unsupported<T>(what: impl Into<String>) -> Res<T> {
    Err(Exc::Unsupported(what.into()))
}

/// The limits one VM runs under.
#[derive(Debug, Clone)]
pub struct Options {
    /// Instructions all threads may run before the run stops.
    pub max_instructions: u64,
    /// Frames one thread may nest before `StackOverflowError`.
    pub max_frames: usize,
    /// Slots one thread's stack may hold before `StackOverflowError`.
    pub max_stack_slots: usize,
    /// Objects the heap may hold before `OutOfMemoryError`.
    pub max_objects: usize,
    /// Elements one array may hold before `OutOfMemoryError`.
    pub max_array: usize,
    /// Native calls back into bytecode that may nest (each is a Rust frame).
    pub max_native_nesting: usize,
    /// `-ea`: assertions on.
    pub assertions: bool,
    /// `-D` system properties.
    pub properties: Vec<(String, String)>,
    /// The virtual clock's epoch, in milliseconds (`currentTimeMillis` at start).
    pub epoch_millis: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_instructions: 2_000_000_000,
            max_frames: 3000,
            max_stack_slots: 1 << 20,
            max_objects: 8 << 20,
            max_array: 64 << 20,
            max_native_nesting: 200,
            assertions: false,
            properties: Vec::new(),
            epoch_millis: 1_767_225_600_000,
        }
    }
}

/// Where class bytes come from: the mod's jars and their libraries.
pub trait ClassSource {
    /// The class file for an internal name (`a/b/C`), if the class path has it.
    fn class_bytes(&self, name: &str) -> Option<Arc<[u8]>>;
    /// A resource's bytes (`getResourceAsStream`).
    fn resource(&self, _name: &str) -> Option<Arc<[u8]>> {
        None
    }
}

/// A class path read from jars in memory: the first jar that has a name keeps
/// it, as a JVM class path resolves one.
#[derive(Default, Clone)]
pub struct JarClassPath {
    classes: HashMap<String, Arc<[u8]>>,
    resources: HashMap<String, Arc<[u8]>>,
}

impl JarClassPath {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a jar's classes and resources, and those of the jars it nests
    /// (`META-INF/jars`, `META-INF/jarjar`), behind what is already there.
    pub fn add_jar(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.add_jar_depth(bytes, 0)
    }

    fn add_jar_depth(&mut self, bytes: &[u8], depth: usize) -> Result<(), String> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
        let mut nested = Vec::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let name = entry.name().to_string();
            if name.ends_with('/') || entry.size() > 64 << 20 {
                continue;
            }
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut data).map_err(|e| e.to_string())?;
            if let Some(class) = name.strip_suffix(".class") {
                if name.starts_with("META-INF/") {
                    continue;
                }
                self.classes.entry(class.to_string()).or_insert_with(|| data.into());
            } else {
                if depth < 2
                    && name.ends_with(".jar")
                    && (name.starts_with("META-INF/jars/") || name.starts_with("META-INF/jarjar/"))
                {
                    nested.push(data.clone());
                }
                self.resources.entry(name).or_insert_with(|| data.into());
            }
        }
        for jar in nested {
            let _ = self.add_jar_depth(&jar, depth + 1);
        }
        Ok(())
    }
}

impl ClassSource for JarClassPath {
    fn class_bytes(&self, name: &str) -> Option<Arc<[u8]>> {
        self.classes.get(name).cloned()
    }
    fn resource(&self, name: &str) -> Option<Arc<[u8]>> {
        self.resources.get(name.trim_start_matches('/')).cloned()
    }
}

/// How a `main` run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum MainOutcome {
    /// `main` returned (and every non-daemon thread ended).
    Returned,
    /// `System.exit(status)`.
    Exited(i32),
    /// An exception left `main`: its class (binary name) and `toString()`.
    Uncaught { class: String, text: String },
    /// The VM stopped: something unsupported, or a limit.
    Stopped(String),
}

/// What a `main` run printed and how it ended.
#[derive(Debug, Clone)]
pub struct MainRun {
    pub outcome: MainOutcome,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub instructions: u64,
}

pub use class::{Class, ClassKind, InitState, Method, MethodBody};
pub use interp::Vm;

/// Runs `main_class`'s `main` as the `java` launcher does (JEP 512's instance
/// and argument-less mains included): stdout and stderr are kept, an uncaught
/// exception is reported as the launcher reports it.
pub fn run_main(
    source: Arc<dyn ClassSource>,
    main_class: &str,
    args: &[String],
    options: Options,
) -> MainRun {
    let mut vm = Vm::new(source, options);
    let outcome = vm.launch_main(&main_class.replace('.', "/"), args);
    MainRun {
        outcome,
        stdout: std::mem::take(&mut vm.stdout),
        stderr: std::mem::take(&mut vm.stderr),
        instructions: vm.instructions,
    }
}

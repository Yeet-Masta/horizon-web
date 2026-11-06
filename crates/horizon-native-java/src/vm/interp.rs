//! The interpreter: methods decoded once into compact instructions, a loop
//! over one slot stack per thread, calls without Rust recursion, and precise
//! exceptions.
use super::class::{ACC_STATIC, ACC_SYNCHRONIZED, ClassKind, InitState, MethodBody};
use super::classfile::{ClassFile, Code, Const};
use super::heap::{Array, Obj, ThrowableData};
use super::{
    ClassId, ClassSource, Data, Exc, MainOutcome, MethodId, NameDesc, ObjId, Options, Res, Value,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Comparisons of the `if` instructions.
#[derive(Clone, Copy, Debug)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Ge,
    Gt,
    Le,
}

impl Cmp {
    fn test(self, a: i32, b: i32) -> bool {
        match self {
            Cmp::Eq => a == b,
            Cmp::Ne => a != b,
            Cmp::Lt => a < b,
            Cmp::Ge => a >= b,
            Cmp::Gt => a > b,
            Cmp::Le => a <= b,
        }
    }
}

/// One decoded instruction. Branch targets are instruction indices; an
/// instruction that names the constant pool keeps the index until its first
/// run resolves it, and is then rewritten into its resolved form.
#[derive(Clone, Copy, Debug)]
pub enum Op {
    Nop,
    Push(Value),
    Push2(Value),
    Ldc(u16),
    LdcRef(ObjId),
    Load(u16),
    Load2(u16),
    Store(u16),
    Store2(u16),
    ALoad(u8),
    AStore(u8),
    Pop,
    Pop2,
    Dup,
    DupX1,
    DupX2,
    Dup2,
    Dup2X1,
    Dup2X2,
    Swap,
    IBin(u8),
    LBin(u8),
    FBin(u8),
    DBin(u8),
    INeg,
    LNeg,
    FNeg,
    DNeg,
    IInc(u16, i16),
    Conv(u8),
    LCmp,
    FCmp(i32),
    DCmp(i32),
    IfZ(Cmp, u32),
    IfICmp(Cmp, u32),
    IfACmp(bool, u32),
    IfNull(bool, u32),
    Goto(u32),
    Jsr(u32),
    Ret(u16),
    Switch(u32),
    Return,
    ReturnValue,
    ReturnWide,
    GetStatic(u16),
    PutStatic(u16),
    GetField(u16),
    PutField(u16),
    GetStaticR(ClassId, u32, bool),
    PutStaticR(ClassId, u32, bool),
    GetFieldR(u32, bool),
    PutFieldR(u32, bool),
    InvokeVirtual(u16),
    InvokeSpecial(u16),
    InvokeStatic(u16),
    InvokeInterface(u16),
    InvokeDynamic(u16),
    /// A resolved `invokespecial`, or an `invokestatic` of an initialized class.
    InvokeDirect(MethodId),
    /// A resolved virtual or interface call: what it selects by, its argument
    /// slots, and the last receiver class and selection (an inline cache).
    InvokeVirtualR(NameDesc, u16, ClassId, MethodId),
    /// A resolved `invokedynamic`: an index into the VM's call sites.
    Indy(u32),
    New(u16),
    NewR(ClassId),
    NewArray(u8),
    ANewArray(u16),
    ANewArrayR(ClassId),
    MultiANewArray(u16, u8),
    ArrayLength,
    AThrow,
    CheckCast(u16),
    CheckCastR(ClassId),
    InstanceOf(u16),
    InstanceOfR(ClassId),
    MonitorEnter,
    MonitorExit,
}

#[derive(Clone, Debug)]
pub struct Switch {
    pub default: u32,
    pub keys: Vec<i32>,
    pub targets: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct HandlerOp {
    pub start: u32,
    pub end: u32,
    pub handler: u32,
    pub catch_type: u16,
}

/// A method decoded for the interpreter.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub ops: Vec<Op>,
    /// The bytecode offset of each instruction (for line numbers).
    pub pcs: Vec<u32>,
    pub switches: Vec<Switch>,
    pub handlers: Vec<HandlerOp>,
    pub max_locals: u16,
    pub max_stack: u16,
    pub code: Arc<Code>,
    pub cf: Arc<ClassFile>,
}

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub method: MethodId,
    /// The next instruction (the one after a pending call).
    pub pc: u32,
    /// Where the frame's locals start on the slot stack.
    pub bp: u32,
}

/// The well-known classes, loaded at start.
#[derive(Debug, Default, Clone)]
pub struct Known {
    pub object: ClassId,
    pub string: ClassId,
    pub class: ClassId,
    pub throwable: ClassId,
}

/// One invokedynamic call site, resolved.
#[derive(Debug, Clone)]
pub enum CallSite {
    /// Allocates an instance of a lambda class with the captured arguments.
    Lambda { class: ClassId, captures: u16, singleton: ObjId },
    /// `StringConcatFactory`: the recipe and the argument descriptors.
    Concat { recipe: Arc<[u16]>, args: Arc<[Arc<str>]>, constants: Arc<[ObjId]> },
    /// A record's `toString`, `equals` or `hashCode` (`ObjectMethods`).
    ObjectMethod { kind: u8, record: ClassId, getters: Arc<[MethodId]> },
    /// `SwitchBootstraps.typeSwitch` and `enumSwitch`.
    TypeSwitch { labels: Arc<[super::indy::Label]>, enum_switch: bool },
}

/// A lambda class's target: what its interface method calls.
#[derive(Debug, Clone)]
pub struct LambdaTarget {
    pub kind: u8,
    pub method: MethodId,
    pub captures: u16,
    /// The interface method's return, as the lambda was instantiated (for
    /// unboxing and boxing at the call).
    pub sam_ret: Arc<str>,
}

pub struct Vm {
    pub options: Options,
    pub(crate) source: Arc<dyn ClassSource>,
    pub(crate) objects: Vec<Option<Obj>>,
    pub(crate) free: Vec<ObjId>,
    pub(crate) allocated: u64,
    pub(crate) classes: Vec<super::Class>,
    pub(crate) class_by_name: HashMap<Arc<str>, ClassId>,
    pub(crate) loading: Vec<Arc<str>>,
    pub(crate) methods: Vec<super::Method>,
    pub(crate) nd_ids: HashMap<(Arc<str>, Arc<str>), NameDesc>,
    pub(crate) nd_names: Vec<(Arc<str>, Arc<str>)>,
    pub(crate) interned: HashMap<Box<[u16]>, ObjId>,
    pub(crate) pinned: Vec<ObjId>,
    pub(crate) known: Known,
    pub(crate) hash_state: u64,
    pub(crate) call_sites: Vec<CallSite>,
    pub(crate) lambdas: Vec<LambdaTarget>,
    pub(crate) catch_cache: HashMap<(usize, u16), ClassId>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub instructions: u64,
    pub(crate) stack: Vec<Value>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) native_depth: usize,
    /// The native method running (signature-polymorphic natives read their
    /// call's descriptor from it).
    pub(crate) native_method: MethodId,
    pub(crate) current_thread: usize,
    /// Per-VM state of the native library (system properties, the main
    /// thread's object, the virtual clock, monitors...).
    pub(crate) jdk: super::jdk::VmState,
}

impl Vm {
    pub fn new(source: Arc<dyn ClassSource>, options: Options) -> Vm {
        let mut vm = Vm {
            options,
            source,
            objects: vec![None],
            free: Vec::new(),
            allocated: 0,
            classes: Vec::new(),
            class_by_name: HashMap::new(),
            loading: Vec::new(),
            methods: Vec::new(),
            nd_ids: HashMap::new(),
            nd_names: Vec::new(),
            interned: HashMap::new(),
            pinned: Vec::new(),
            known: Known::default(),
            hash_state: 0x9E37_79B9_7F4A_7C15,
            call_sites: Vec::new(),
            lambdas: Vec::new(),
            catch_cache: HashMap::new(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            instructions: 0,
            stack: Vec::with_capacity(4096),
            frames: Vec::with_capacity(256),
            native_depth: 0,
            native_method: 0,
            current_thread: 0,
            jdk: Default::default(),
        };
        vm.boot();
        vm
    }

    fn boot(&mut self) {
        for (name, slot) in [
            ("java/lang/Object", 0),
            ("java/lang/String", 1),
            ("java/lang/Class", 2),
            ("java/lang/Throwable", 3),
        ] {
            let c = self.load(name).unwrap_or(0);
            match slot {
                0 => self.known.object = c,
                1 => self.known.string = c,
                2 => self.known.class = c,
                _ => self.known.throwable = c,
            }
        }
    }

    // ---------------------------------------------------------------- calls

    /// Calls a method with logical arguments (one value per parameter, `this`
    /// first for an instance method) and answers its result (`Top` for void).
    pub fn invoke(&mut self, m: MethodId, args: &[Value]) -> Res<Value> {
        if self.native_depth >= self.options.max_native_nesting {
            return Err(self.throw_new("java/lang/StackOverflowError", None));
        }
        let base = self.frames.len();
        let stack_base = self.stack.len();
        for &a in args {
            self.stack.push(a);
            if a.is_wide() {
                self.stack.push(Value::Top);
            }
        }
        let slots = self.stack.len() - stack_base;
        self.native_depth += 1;
        let result = (|| {
            if self.invoke_from_stack(m, slots)? {
                self.execute(base)?;
            }
            let method = self.method(m);
            Ok(if method.ret == b'V' {
                Value::Top
            } else if method.returns_wide() {
                self.stack.pop();
                self.stack.pop().unwrap_or_default()
            } else {
                self.stack.pop().unwrap_or_default()
            })
        })();
        self.native_depth -= 1;
        if result.is_err() {
            self.stack.truncate(stack_base);
            self.frames.truncate(base);
        }
        result
    }

    /// Calls the method a virtual call on `receiver` selects.
    pub fn invoke_virtual(&mut self, receiver: ObjId, name: &str, desc: &str, args: &[Value]) -> Res<Value> {
        let Some(class) = self.class_of(receiver) else {
            return Err(self.npe(Some(&format!("Cannot invoke \"{name}()\" because value is null"))));
        };
        let nd = self.nd(name, desc);
        let m = self.select(class, nd)?;
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(Value::Ref(receiver));
        all.extend_from_slice(args);
        self.invoke(m, &all)
    }

    /// Calls a static method by name (initializing its class).
    pub fn invoke_static(&mut self, class: &str, name: &str, desc: &str, args: &[Value]) -> Res<Value> {
        let c = self.load(class)?;
        self.ensure_init(c)?;
        let Some(m) = self.find_method(c, name, desc) else {
            return Err(Exc::Internal(format!("no method {class}.{name}{desc}")));
        };
        self.invoke(m, args)
    }

    /// Starts a method whose `slots` argument slots are on top of the stack:
    /// a bytecode method gets a frame (true), any other runs to completion and
    /// leaves its result on the stack (false).
    pub(crate) fn invoke_from_stack(&mut self, m: MethodId, slots: usize) -> Res<bool> {
        let body = self.methods[m as usize].body.clone();
        match body {
            MethodBody::Bytecode(_) => {
                if self.methods[m as usize].decoded.is_none() {
                    let decoded = self.decode(m)?;
                    self.methods[m as usize].decoded = Some(Box::new(decoded));
                }
                let d = self.methods[m as usize].decoded.as_ref().unwrap();
                let (max_locals, max_stack) = (d.max_locals as usize, d.max_stack as usize);
                if self.frames.len() >= self.options.max_frames
                    || self.stack.len() + max_locals + max_stack > self.options.max_stack_slots
                {
                    self.stack.truncate(self.stack.len() - slots.min(self.stack.len()));
                    return Err(self.throw_new("java/lang/StackOverflowError", None));
                }
                let bp = self.stack.len() - slots;
                if max_locals > slots {
                    self.stack.resize(bp + max_locals, Value::Top);
                } else {
                    self.stack.truncate(bp + max_locals);
                }
                self.frames.push(Frame {
                    method: m,
                    pc: 0,
                    bp: bp as u32,
                });
                if self.methods[m as usize].access & ACC_SYNCHRONIZED != 0 {
                    let lock = self.sync_object(m, bp)?;
                    self.monitor_enter(lock)?;
                }
                Ok(true)
            }
            MethodBody::Native(f) => {
                let start = self.stack.len() - slots;
                let args: Vec<Value> = self.stack[start..]
                    .iter()
                    .copied()
                    .filter(|v| !matches!(v, Value::Top))
                    .collect();
                self.stack.truncate(start);
                self.native_method = m;
                let result = f(self, &args)?;
                self.push_result(m, result);
                Ok(false)
            }
            MethodBody::Lambda(index) => {
                let start = self.stack.len() - slots;
                let args: Vec<Value> = self.stack[start..]
                    .iter()
                    .copied()
                    .filter(|v| !matches!(v, Value::Top))
                    .collect();
                self.stack.truncate(start);
                let result = self.call_lambda(index, &args)?;
                self.push_result(m, result);
                Ok(false)
            }
            MethodBody::Proxy => {
                let start = self.stack.len() - slots;
                let args: Vec<Value> = self.stack[start..]
                    .iter()
                    .copied()
                    .filter(|v| !matches!(v, Value::Top))
                    .collect();
                self.stack.truncate(start);
                let result = super::jdk::reflect::call_proxy(self, m, &args)?;
                self.push_result(m, result);
                Ok(false)
            }
            MethodBody::Abstract => {
                self.stack.truncate(self.stack.len() - slots);
                let method = self.method(m);
                let msg = format!(
                    "'{}' in {}",
                    self.pretty_method(&method.name.clone(), &method.desc.clone()),
                    self.class(method.class).java_name()
                );
                if method.access & super::class::ACC_NATIVE != 0 {
                    let msg = format!("'{}'", self.pretty_method(&method.name.clone(), &method.desc.clone()));
                    return Err(self.throw_new("java/lang/UnsatisfiedLinkError", Some(&msg)));
                }
                Err(self.throw_new("java/lang/AbstractMethodError", Some(&msg)))
            }
        }
    }

    fn push_result(&mut self, m: MethodId, result: Value) {
        let ret = self.methods[m as usize].ret;
        match ret {
            b'V' => {}
            b'J' | b'D' => {
                self.stack.push(result);
                self.stack.push(Value::Top);
            }
            b'Z' | b'B' | b'C' | b'S' | b'I' => self.stack.push(Value::Int(result.i())),
            _ => self.stack.push(result),
        }
    }

    fn sync_object(&mut self, m: MethodId, bp: usize) -> Res<ObjId> {
        if self.methods[m as usize].access & ACC_STATIC != 0 {
            let class = self.methods[m as usize].class;
            self.mirror(class)
        } else {
            Ok(self.stack[bp].r())
        }
    }

    // ---------------------------------------------------------------- decode

    fn decode(&mut self, m: MethodId) -> Res<Decoded> {
        let class = self.methods[m as usize].class;
        let Some(cf) = self.class(class).class_file().cloned() else {
            return Err(Exc::Internal("bytecode method of a class with no class file".into()));
        };
        let MethodBody::Bytecode(code) = self.methods[m as usize].body.clone() else {
            return Err(Exc::Internal("decode of a non-bytecode method".into()));
        };
        match decode_code(&code, &cf) {
            Ok(d) => Ok(d),
            Err(msg) => {
                let method = self.method(m);
                let msg = format!(
                    "(class: {}, method: {} signature: {}) {msg}",
                    self.class(class).java_name(),
                    method.name,
                    method.desc
                );
                Err(self.throw_new("java/lang/VerifyError", Some(&msg)))
            }
        }
    }

    // ---------------------------------------------------------------- run

    /// Runs the current thread's frames until the frame stack is back to
    /// `base` frames.
    pub(crate) fn execute(&mut self, base: usize) -> Res<()> {
        'frames: loop {
            if self.frames.len() <= base {
                return Ok(());
            }
            let fi = self.frames.len() - 1;
            let Frame { method: m, pc: start, bp } = self.frames[fi];
            let bp = bp as usize;
            let mut pc = start as usize;
            macro_rules! tri {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(e) => {
                            self.frames[fi].pc = pc as u32;
                            match self.unwind(e, base) {
                                Ok(()) => continue 'frames,
                                Err(e) => return Err(e),
                            }
                        }
                    }
                };
            }
            macro_rules! pop {
                () => {
                    self.stack.pop().unwrap_or_default()
                };
            }
            macro_rules! pop2 {
                () => {{
                    self.stack.pop();
                    self.stack.pop().unwrap_or_default()
                }};
            }
            macro_rules! push2 {
                ($v:expr) => {{
                    self.stack.push($v);
                    self.stack.push(Value::Top);
                }};
            }
            loop {
                self.instructions += 1;
                if self.instructions >= self.options.max_instructions {
                    self.frames[fi].pc = pc as u32;
                    return Err(Exc::Limit(format!(
                        "instruction budget of {} spent",
                        self.options.max_instructions
                    )));
                }
                let Some(d) = self.methods[m as usize].decoded.as_ref() else {
                    return Err(Exc::Internal("frame of an undecoded method".into()));
                };
                let Some(&op) = d.ops.get(pc) else {
                    return Err(Exc::Internal("fell off the end of the code".into()));
                };
                pc += 1;
                match op {
                    Op::Nop => {}
                    Op::Push(v) => self.stack.push(v),
                    Op::Push2(v) => push2!(v),
                    Op::LdcRef(r) => self.stack.push(Value::Ref(r)),
                    Op::Ldc(index) => {
                        self.frames[fi].pc = pc as u32;
                        let v = tri!(self.ldc(m, index));
                        if let Value::Ref(r) = v {
                            self.patch(m, pc - 1, Op::LdcRef(r));
                        }
                        if v.is_wide() {
                            push2!(v)
                        } else {
                            self.stack.push(v)
                        }
                    }
                    Op::Load(i) => {
                        let v = self.stack[bp + i as usize];
                        self.stack.push(v);
                    }
                    Op::Load2(i) => {
                        let v = self.stack[bp + i as usize];
                        push2!(v);
                    }
                    Op::Store(i) => {
                        let v = pop!();
                        self.stack[bp + i as usize] = v;
                    }
                    Op::Store2(i) => {
                        let v = pop2!();
                        self.stack[bp + i as usize] = v;
                        self.stack[bp + i as usize + 1] = Value::Top;
                    }
                    Op::ALoad(kind) => {
                        let i = pop!().i();
                        let a = pop!().r();
                        if a == 0 {
                            self.frames[fi].pc = pc as u32;
                            let msg = self.helpful_npe(m, pc - 1);
                            tri!(Err(self.npe(msg.as_deref())))
                        }
                        let v = tri!(self.array_load(a, i));
                        if kind == b'J' || kind == b'D' {
                            push2!(v)
                        } else {
                            self.stack.push(v)
                        }
                    }
                    Op::AStore(kind) => {
                        let v = if kind == b'J' || kind == b'D' { pop2!() } else { pop!() };
                        let i = pop!().i();
                        let a = pop!().r();
                        if a == 0 {
                            self.frames[fi].pc = pc as u32;
                            let msg = self.helpful_npe(m, pc - 1);
                            tri!(Err(self.npe(msg.as_deref())))
                        }
                        tri!(self.array_store(a, i, v));
                    }
                    Op::Pop => {
                        self.stack.pop();
                    }
                    Op::Pop2 => {
                        self.stack.pop();
                        self.stack.pop();
                    }
                    Op::Dup => {
                        let v = *self.stack.last().unwrap_or(&Value::Top);
                        self.stack.push(v);
                    }
                    Op::DupX1 => {
                        let v1 = pop!();
                        let v2 = pop!();
                        self.stack.extend_from_slice(&[v1, v2, v1]);
                    }
                    Op::DupX2 => {
                        let v1 = pop!();
                        let v2 = pop!();
                        let v3 = pop!();
                        self.stack.extend_from_slice(&[v1, v3, v2, v1]);
                    }
                    Op::Dup2 => {
                        let n = self.stack.len();
                        if n >= 2 {
                            let (a, b) = (self.stack[n - 2], self.stack[n - 1]);
                            self.stack.extend_from_slice(&[a, b]);
                        }
                    }
                    Op::Dup2X1 => {
                        let v1 = pop!();
                        let v2 = pop!();
                        let v3 = pop!();
                        self.stack.extend_from_slice(&[v2, v1, v3, v2, v1]);
                    }
                    Op::Dup2X2 => {
                        let v1 = pop!();
                        let v2 = pop!();
                        let v3 = pop!();
                        let v4 = pop!();
                        self.stack.extend_from_slice(&[v2, v1, v4, v3, v2, v1]);
                    }
                    Op::Swap => {
                        let v1 = pop!();
                        let v2 = pop!();
                        self.stack.extend_from_slice(&[v1, v2]);
                    }
                    Op::IBin(o) => {
                        let b = pop!().i();
                        let a = pop!().i();
                        let r = match o {
                            0 => a.wrapping_add(b),
                            1 => a.wrapping_sub(b),
                            2 => a.wrapping_mul(b),
                            3 | 4 => {
                                if b == 0 {
                                    self.frames[fi].pc = pc as u32;
                                    tri!(Err(self.throw_new("java/lang/ArithmeticException", Some("/ by zero"))))
                                } else if o == 3 {
                                    a.wrapping_div(b)
                                } else {
                                    a.wrapping_rem(b)
                                }
                            }
                            5 => a.wrapping_shl(b as u32 & 31),
                            6 => a.wrapping_shr(b as u32 & 31),
                            7 => ((a as u32) >> (b as u32 & 31)) as i32,
                            8 => a & b,
                            9 => a | b,
                            _ => a ^ b,
                        };
                        self.stack.push(Value::Int(r));
                    }
                    Op::LBin(o) => {
                        // Shifts take an int count.
                        let (a, b) = if (5..=7).contains(&o) {
                            let b = pop!().i() as i64;
                            (pop2!().j(), b)
                        } else {
                            let b = pop2!().j();
                            (pop2!().j(), b)
                        };
                        let r = match o {
                            0 => a.wrapping_add(b),
                            1 => a.wrapping_sub(b),
                            2 => a.wrapping_mul(b),
                            3 | 4 => {
                                if b == 0 {
                                    self.frames[fi].pc = pc as u32;
                                    tri!(Err(self.throw_new("java/lang/ArithmeticException", Some("/ by zero"))))
                                } else if o == 3 {
                                    a.wrapping_div(b)
                                } else {
                                    a.wrapping_rem(b)
                                }
                            }
                            5 => a.wrapping_shl(b as u32 & 63),
                            6 => a.wrapping_shr(b as u32 & 63),
                            7 => ((a as u64) >> (b as u32 & 63)) as i64,
                            8 => a & b,
                            9 => a | b,
                            _ => a ^ b,
                        };
                        push2!(Value::Long(r));
                    }
                    Op::FBin(o) => {
                        let b = pop!().f();
                        let a = pop!().f();
                        let r = match o {
                            0 => a + b,
                            1 => a - b,
                            2 => a * b,
                            3 => a / b,
                            _ => a % b,
                        };
                        self.stack.push(Value::Float(r));
                    }
                    Op::DBin(o) => {
                        let b = pop2!().d();
                        let a = pop2!().d();
                        let r = match o {
                            0 => a + b,
                            1 => a - b,
                            2 => a * b,
                            3 => a / b,
                            _ => a % b,
                        };
                        push2!(Value::Double(r));
                    }
                    Op::INeg => {
                        let a = pop!().i();
                        self.stack.push(Value::Int(a.wrapping_neg()));
                    }
                    Op::LNeg => {
                        let a = pop2!().j();
                        push2!(Value::Long(a.wrapping_neg()));
                    }
                    Op::FNeg => {
                        let a = pop!().f();
                        self.stack.push(Value::Float(-a));
                    }
                    Op::DNeg => {
                        let a = pop2!().d();
                        push2!(Value::Double(-a));
                    }
                    Op::IInc(i, by) => {
                        let slot = &mut self.stack[bp + i as usize];
                        *slot = Value::Int(slot.i().wrapping_add(by as i32));
                    }
                    Op::Conv(c) => {
                        match c {
                            0 => {
                                let a = pop!().i();
                                push2!(Value::Long(a as i64))
                            }
                            1 => {
                                let a = pop!().i();
                                self.stack.push(Value::Float(a as f32))
                            }
                            2 => {
                                let a = pop!().i();
                                push2!(Value::Double(a as f64))
                            }
                            3 => {
                                let a = pop2!().j();
                                self.stack.push(Value::Int(a as i32))
                            }
                            4 => {
                                let a = pop2!().j();
                                self.stack.push(Value::Float(a as f32))
                            }
                            5 => {
                                let a = pop2!().j();
                                push2!(Value::Double(a as f64))
                            }
                            // Rust's float-to-int casts saturate and map NaN to 0, as Java's do.
                            6 => {
                                let a = pop!().f();
                                self.stack.push(Value::Int(a as i32))
                            }
                            7 => {
                                let a = pop!().f();
                                push2!(Value::Long(a as i64))
                            }
                            8 => {
                                let a = pop!().f();
                                push2!(Value::Double(a as f64))
                            }
                            9 => {
                                let a = pop2!().d();
                                self.stack.push(Value::Int(a as i32))
                            }
                            10 => {
                                let a = pop2!().d();
                                push2!(Value::Long(a as i64))
                            }
                            11 => {
                                let a = pop2!().d();
                                self.stack.push(Value::Float(a as f32))
                            }
                            12 => {
                                let a = pop!().i();
                                self.stack.push(Value::Int(a as i8 as i32))
                            }
                            13 => {
                                let a = pop!().i();
                                self.stack.push(Value::Int(a as u16 as i32))
                            }
                            _ => {
                                let a = pop!().i();
                                self.stack.push(Value::Int(a as i16 as i32))
                            }
                        }
                    }
                    Op::LCmp => {
                        let b = pop2!().j();
                        let a = pop2!().j();
                        self.stack.push(Value::Int((a > b) as i32 - (a < b) as i32));
                    }
                    Op::FCmp(nan) => {
                        let b = pop!().f();
                        let a = pop!().f();
                        let r = if a.is_nan() || b.is_nan() {
                            nan
                        } else {
                            (a > b) as i32 - (a < b) as i32
                        };
                        self.stack.push(Value::Int(r));
                    }
                    Op::DCmp(nan) => {
                        let b = pop2!().d();
                        let a = pop2!().d();
                        let r = if a.is_nan() || b.is_nan() {
                            nan
                        } else {
                            (a > b) as i32 - (a < b) as i32
                        };
                        self.stack.push(Value::Int(r));
                    }
                    Op::IfZ(c, t) => {
                        if c.test(pop!().i(), 0) {
                            pc = t as usize;
                        }
                    }
                    Op::IfICmp(c, t) => {
                        let b = pop!().i();
                        let a = pop!().i();
                        if c.test(a, b) {
                            pc = t as usize;
                        }
                    }
                    Op::IfACmp(eq, t) => {
                        let b = pop!().r();
                        let a = pop!().r();
                        if (a == b) == eq {
                            pc = t as usize;
                        }
                    }
                    Op::IfNull(null, t) => {
                        if (pop!().r() == 0) == null {
                            pc = t as usize;
                        }
                    }
                    Op::Goto(t) => pc = t as usize,
                    Op::Jsr(t) => {
                        self.stack.push(Value::Int(pc as i32));
                        pc = t as usize;
                    }
                    Op::Ret(i) => pc = self.stack[bp + i as usize].i() as usize,
                    Op::Switch(s) => {
                        let key = pop!().i();
                        let d = self.methods[m as usize].decoded.as_ref().unwrap();
                        let sw = &d.switches[s as usize];
                        pc = match sw.keys.binary_search(&key) {
                            Ok(i) => sw.targets[i],
                            Err(_) => sw.default,
                        } as usize;
                    }
                    Op::Return | Op::ReturnValue | Op::ReturnWide => {
                        self.frames[fi].pc = pc as u32;
                        let result = match op {
                            Op::Return => None,
                            Op::ReturnValue => Some((pop!(), false)),
                            _ => Some((pop2!(), true)),
                        };
                        if self.methods[m as usize].access & ACC_SYNCHRONIZED != 0 {
                            let lock = tri!(self.sync_object(m, bp));
                            tri!(self.monitor_exit(lock));
                        }
                        self.stack.truncate(bp);
                        self.frames.pop();
                        if let Some((v, wide)) = result {
                            let v = match (self.methods[m as usize].ret, v) {
                                // `boolean`, `byte`, `char` and `short` returns narrow.
                                (b'Z', v) => Value::Int(v.i() & 1),
                                (b'B', v) => Value::Int(v.i() as i8 as i32),
                                (b'C', v) => Value::Int(v.i() as u16 as i32),
                                (b'S', v) => Value::Int(v.i() as i16 as i32),
                                (_, v) => v,
                            };
                            self.stack.push(v);
                            if wide {
                                self.stack.push(Value::Top);
                            }
                        }
                        continue 'frames;
                    }
                    Op::GetStatic(index) | Op::PutStatic(index) => {
                        self.frames[fi].pc = pc as u32;
                        let (class, slot, wide) = tri!(self.resolve_static(m, index));
                        tri!(self.ensure_init(class));
                        let put = matches!(op, Op::PutStatic(_));
                        if self.class(class).state == InitState::Initialized {
                            self.patch(
                                m,
                                pc - 1,
                                if put {
                                    Op::PutStaticR(class, slot, wide)
                                } else {
                                    Op::GetStaticR(class, slot, wide)
                                },
                            );
                        }
                        if put {
                            let v = if wide { pop2!() } else { pop!() };
                            self.classes[class as usize].statics[slot as usize] = v;
                        } else {
                            let v = self.classes[class as usize].statics[slot as usize];
                            self.stack.push(v);
                            if wide {
                                self.stack.push(Value::Top);
                            }
                        }
                    }
                    Op::GetStaticR(class, slot, wide) => {
                        let v = self.classes[class as usize].statics[slot as usize];
                        self.stack.push(v);
                        if wide {
                            self.stack.push(Value::Top);
                        }
                    }
                    Op::PutStaticR(class, slot, wide) => {
                        let v = if wide { pop2!() } else { pop!() };
                        self.classes[class as usize].statics[slot as usize] = v;
                    }
                    Op::GetField(index) | Op::PutField(index) => {
                        self.frames[fi].pc = pc as u32;
                        let (slot, wide) = tri!(self.resolve_instance_field(m, index));
                        let put = matches!(op, Op::PutField(_));
                        self.patch(
                            m,
                            pc - 1,
                            if put {
                                Op::PutFieldR(slot, wide)
                            } else {
                                Op::GetFieldR(slot, wide)
                            },
                        );
                        pc -= 1;
                    }
                    Op::GetFieldR(slot, wide) => {
                        let r = pop!().r();
                        let v = match self.obj(r).and_then(|o| o.fields.get(slot as usize)) {
                            Some(&v) => v,
                            None => {
                                self.frames[fi].pc = pc as u32;
                                let msg = self.helpful_npe(m, pc - 1);
                                tri!(Err(self.npe(msg.as_deref())))
                            }
                        };
                        self.stack.push(v);
                        if wide {
                            self.stack.push(Value::Top);
                        }
                    }
                    Op::PutFieldR(slot, wide) => {
                        let v = if wide { pop2!() } else { pop!() };
                        let r = pop!().r();
                        match self.obj_mut(r).and_then(|o| o.fields.get_mut(slot as usize)) {
                            Some(f) => *f = v,
                            None => {
                                self.frames[fi].pc = pc as u32;
                                let msg = self.helpful_npe(m, pc - 1);
                                tri!(Err(self.npe(msg.as_deref())))
                            }
                        }
                    }
                    Op::InvokeVirtual(index) | Op::InvokeInterface(index) => {
                        self.frames[fi].pc = pc as u32;
                        let (nd, slots, direct) = tri!(self.resolve_virtual(m, index));
                        match direct {
                            Some(target) => self.patch(m, pc - 1, Op::InvokeDirect(target)),
                            None => self.patch(m, pc - 1, Op::InvokeVirtualR(nd, slots, u32::MAX, 0)),
                        }
                        pc -= 1;
                    }
                    Op::InvokeVirtualR(nd, slots, cached_class, cached) => {
                        self.frames[fi].pc = pc as u32;
                        let at = self.stack.len().saturating_sub(slots as usize + 1);
                        let receiver = self.stack.get(at).map_or(0, |v| v.r());
                        let Some(class) = self.class_of(receiver) else {
                            let msg = self.helpful_npe(m, pc - 1);
                            tri!(Err(self.npe(msg.as_deref())))
                        };
                        let target = if class == cached_class {
                            cached
                        } else {
                            let t = tri!(self.select(class, nd));
                            self.patch(m, pc - 1, Op::InvokeVirtualR(nd, slots, class, t));
                            t
                        };
                        if tri!(self.invoke_from_stack(target, slots as usize + 1)) {
                            continue 'frames;
                        }
                    }
                    Op::InvokeSpecial(index) => {
                        self.frames[fi].pc = pc as u32;
                        let target = tri!(self.resolve_special(m, index));
                        self.patch(m, pc - 1, Op::InvokeDirect(target));
                        pc -= 1;
                    }
                    Op::InvokeStatic(index) => {
                        self.frames[fi].pc = pc as u32;
                        let target = tri!(self.resolve_static_method(m, index));
                        let class = self.method(target).class;
                        tri!(self.ensure_init(class));
                        if self.class(class).state == InitState::Initialized {
                            self.patch(m, pc - 1, Op::InvokeDirect(target));
                        }
                        let slots = self.method(target).arg_slots as usize;
                        if tri!(self.invoke_from_stack(target, slots)) {
                            continue 'frames;
                        }
                    }
                    Op::InvokeDirect(target) => {
                        self.frames[fi].pc = pc as u32;
                        let method = &self.methods[target as usize];
                        let slots = method.arg_slots as usize + (method.access & ACC_STATIC == 0) as usize;
                        if method.access & ACC_STATIC == 0 {
                            let at = self.stack.len().saturating_sub(slots);
                            if self.stack.get(at).map_or(0, |v| v.r()) == 0 {
                                let msg = self.helpful_npe(m, pc - 1);
                                tri!(Err(self.npe(msg.as_deref())))
                            }
                        }
                        if tri!(self.invoke_from_stack(target, slots)) {
                            continue 'frames;
                        }
                    }
                    Op::InvokeDynamic(index) => {
                        self.frames[fi].pc = pc as u32;
                        let site = tri!(self.resolve_indy(m, index));
                        self.patch(m, pc - 1, Op::Indy(site));
                        pc -= 1;
                    }
                    Op::Indy(site) => {
                        self.frames[fi].pc = pc as u32;
                        tri!(self.run_call_site(site as usize));
                    }
                    Op::New(index) => {
                        self.frames[fi].pc = pc as u32;
                        let class = tri!(self.resolve_class(m, index));
                        let cls = self.class(class);
                        if cls.is_interface() || cls.access & super::class::ACC_ABSTRACT != 0 {
                            let name = cls.java_name();
                            tri!(Err(self.throw_new("java/lang/InstantiationError", Some(&name))));
                        }
                        tri!(self.ensure_init(class));
                        if self.class(class).state == InitState::Initialized {
                            self.patch(m, pc - 1, Op::NewR(class));
                        }
                        let r = tri!(self.new_instance(class));
                        self.stack.push(Value::Ref(r));
                    }
                    Op::NewR(class) => {
                        self.frames[fi].pc = pc as u32;
                        let r = tri!(self.new_instance(class));
                        self.stack.push(Value::Ref(r));
                    }
                    Op::NewArray(elem) => {
                        self.frames[fi].pc = pc as u32;
                        let len = pop!().i();
                        let class = tri!(self.array_class(&format!("[{}", elem as char)));
                        let r = tri!(self.new_array(class, len));
                        self.stack.push(Value::Ref(r));
                    }
                    Op::ANewArray(index) => {
                        self.frames[fi].pc = pc as u32;
                        let comp = tri!(self.resolve_class(m, index));
                        let class = tri!(self.array_class_of(comp));
                        self.patch(m, pc - 1, Op::ANewArrayR(class));
                        let len = pop!().i();
                        let r = tri!(self.new_array(class, len));
                        self.stack.push(Value::Ref(r));
                    }
                    Op::ANewArrayR(class) => {
                        self.frames[fi].pc = pc as u32;
                        let len = pop!().i();
                        let r = tri!(self.new_array(class, len));
                        self.stack.push(Value::Ref(r));
                    }
                    Op::MultiANewArray(index, dims) => {
                        self.frames[fi].pc = pc as u32;
                        let class = tri!(self.resolve_class(m, index));
                        let mut lens = vec![0i32; dims as usize];
                        for l in lens.iter_mut().rev() {
                            *l = pop!().i();
                        }
                        if let Some(&neg) = lens.iter().find(|&&l| l < 0) {
                            tri!(Err(self.throw_new("java/lang/NegativeArraySizeException", Some(&neg.to_string()))));
                        }
                        let r = tri!(self.multi_array(class, &lens));
                        self.stack.push(Value::Ref(r));
                    }
                    Op::ArrayLength => {
                        let r = pop!().r();
                        match self.array(r) {
                            Some(a) => {
                                let n = a.len() as i32;
                                self.stack.push(Value::Int(n));
                            }
                            None => {
                                self.frames[fi].pc = pc as u32;
                                let msg = self.helpful_npe(m, pc - 1);
                                tri!(Err(self.npe(msg.as_deref())))
                            }
                        }
                    }
                    Op::AThrow => {
                        self.frames[fi].pc = pc as u32;
                        let r = pop!().r();
                        if r == 0 {
                            let msg = self.helpful_npe(m, pc - 1);
                            tri!(Err(self.npe(msg.as_deref())));
                        }
                        tri!(Err(Exc::Throw(r)));
                    }
                    Op::CheckCast(index) | Op::InstanceOf(index) => {
                        self.frames[fi].pc = pc as u32;
                        let class = tri!(self.resolve_class(m, index));
                        let resolved = if matches!(op, Op::CheckCast(_)) {
                            Op::CheckCastR(class)
                        } else {
                            Op::InstanceOfR(class)
                        };
                        self.patch(m, pc - 1, resolved);
                        pc -= 1;
                    }
                    Op::CheckCastR(class) => {
                        let r = self.stack.last().map_or(0, |v| v.r());
                        if r != 0 && !self.instance_of(r, class) {
                            self.frames[fi].pc = pc as u32;
                            let msg = self.class_cast_message(r, class);
                            tri!(Err(self.throw_new("java/lang/ClassCastException", Some(&msg))));
                        }
                    }
                    Op::InstanceOfR(class) => {
                        let r = pop!().r();
                        let yes = r != 0 && self.instance_of(r, class);
                        self.stack.push(Value::Int(yes as i32));
                    }
                    Op::MonitorEnter => {
                        self.frames[fi].pc = pc as u32;
                        let r = pop!().r();
                        if r == 0 {
                            let msg = self.helpful_npe(m, pc - 1);
                            tri!(Err(self.npe(msg.as_deref())));
                        }
                        tri!(self.monitor_enter(r));
                    }
                    Op::MonitorExit => {
                        self.frames[fi].pc = pc as u32;
                        let r = pop!().r();
                        if r == 0 {
                            tri!(Err(self.npe(None)));
                        }
                        tri!(self.monitor_exit(r));
                    }
                }
            }
        }
    }

    fn patch(&mut self, m: MethodId, at: usize, op: Op) {
        if let Some(d) = self.methods[m as usize].decoded.as_mut()
            && let Some(slot) = d.ops.get_mut(at)
        {
            *slot = op;
        }
    }

    /// Finds a handler for a propagating exception in the current thread's
    /// frames above `base`, popping the frames it leaves.
    fn unwind(&mut self, e: Exc, base: usize) -> Res<()> {
        let Exc::Throw(ex) = e else {
            return Err(e);
        };
        while self.frames.len() > base {
            let f = *self.frames.last().unwrap();
            if let Some(handler) = self.find_handler(f.method, f.pc.saturating_sub(1), ex)? {
                let d = self.methods[f.method as usize].decoded.as_ref().unwrap();
                let keep = f.bp as usize + d.max_locals as usize;
                self.stack.truncate(keep);
                self.stack.push(Value::Ref(ex));
                self.frames.last_mut().unwrap().pc = handler;
                return Ok(());
            }
            if self.methods[f.method as usize].access & ACC_SYNCHRONIZED != 0 {
                let lock = self.sync_object(f.method, f.bp as usize)?;
                let _ = self.monitor_exit(lock);
            }
            self.stack.truncate(f.bp as usize);
            self.frames.pop();
        }
        Err(Exc::Throw(ex))
    }

    fn find_handler(&mut self, m: MethodId, at: u32, ex: ObjId) -> Res<Option<u32>> {
        let Some(d) = self.methods[m as usize].decoded.as_ref() else {
            return Ok(None);
        };
        let candidates: Vec<(u32, u16)> = d
            .handlers
            .iter()
            .filter(|h| h.start <= at && at < h.end)
            .map(|h| (h.handler, h.catch_type))
            .collect();
        let cf = d.cf.clone();
        for (handler, catch) in candidates {
            if catch == 0 {
                return Ok(Some(handler));
            }
            let key = (Arc::as_ptr(&cf) as *const u8 as usize, catch);
            let class = match self.catch_cache.get(&key) {
                Some(&c) => c,
                None => {
                    let name = cf.class_name(catch).map_err(|e| Exc::Internal(e.0))?;
                    let c = self.load(&name)?;
                    self.catch_cache.insert(key, c);
                    c
                }
            };
            if self.instance_of(ex, class) {
                return Ok(Some(handler));
            }
        }
        Ok(None)
    }

    // ---------------------------------------------------------------- arrays

    fn array_load(&mut self, a: ObjId, i: i32) -> Res<Value> {
        match self.array(a) {
            None => Err(self.npe(None)),
            Some(arr) => match arr.get(i as usize).filter(|_| i >= 0) {
                Some(v) => Ok(v),
                None => {
                    let msg = format!("Index {i} out of bounds for length {}", arr.len());
                    Err(self.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)))
                }
            },
        }
    }

    fn array_store(&mut self, a: ObjId, i: i32, v: Value) -> Res<()> {
        let Some(arr) = self.array(a) else {
            return Err(self.npe(None));
        };
        let len = arr.len();
        if i < 0 || i as usize >= len {
            let msg = format!("Index {i} out of bounds for length {len}");
            return Err(self.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
        }
        if let (Value::Ref(r), Array::L(_)) = (v, arr)
            && r != 0
        {
            let array_class = self.class_of(a).unwrap();
            if let ClassKind::Array { component, .. } = self.class(array_class).kind
                && !self.instance_of(r, component)
            {
                let name = self.class(self.class_of(r).unwrap()).java_name();
                return Err(self.throw_new("java/lang/ArrayStoreException", Some(&name)));
            }
        }
        self.array_mut(a).unwrap().set(i as usize, v);
        Ok(())
    }

    fn multi_array(&mut self, class: ClassId, lens: &[i32]) -> Res<ObjId> {
        let r = self.new_array(class, lens[0])?;
        if lens.len() > 1
            && let ClassKind::Array { component, .. } = self.class(class).kind
        {
            for i in 0..lens[0] as usize {
                let inner = self.multi_array(component, &lens[1..])?;
                if let Some(Array::L(v)) = self.array_mut(r) {
                    v[i] = inner;
                }
            }
        }
        Ok(r)
    }

    // ---------------------------------------------------------------- resolution

    fn cf_of(&self, m: MethodId) -> Arc<ClassFile> {
        self.methods[m as usize].decoded.as_ref().unwrap().cf.clone()
    }

    fn ldc(&mut self, m: MethodId, index: u16) -> Res<Value> {
        let cf = self.cf_of(m);
        let c = cf.get(index).map_err(|e| Exc::Internal(e.0))?.clone();
        match c {
            Const::Int(v) => Ok(Value::Int(v)),
            Const::Float(v) => Ok(Value::Float(v)),
            Const::Long(v) => Ok(Value::Long(v)),
            Const::Double(v) => Ok(Value::Double(v)),
            Const::Str(s) => {
                let units = cf.utf8(s).map_err(|e| Exc::Internal(e.0))?.units.clone();
                Ok(Value::Ref(self.intern_units(&units)?))
            }
            Const::Class(_) => {
                let name = cf.class_name(index).map_err(|e| Exc::Internal(e.0))?;
                let c = self.class_of_desc_or_name(&name)?;
                Ok(Value::Ref(self.mirror(c)?))
            }
            Const::Dynamic { .. } => self.resolve_condy(&cf, index),
            Const::MethodType(d) => {
                let desc = cf.utf8(d).map_err(|e| Exc::Internal(e.0))?.text.clone();
                super::jdk::invoke::method_type_of(self, &desc).map(Value::Ref)
            }
            Const::MethodHandle { .. } => super::jdk::invoke::handle_constant(self, &cf, index).map(Value::Ref),
            other => Err(Exc::Internal(format!("ldc of {other:?}"))),
        }
    }

    /// A class named in the constant pool: an internal name or an array descriptor.
    pub(crate) fn class_of_desc_or_name(&mut self, name: &str) -> Res<ClassId> {
        if name.starts_with('[') {
            self.array_class(name)
        } else {
            self.load(name)
        }
    }

    pub(crate) fn resolve_class(&mut self, m: MethodId, index: u16) -> Res<ClassId> {
        let cf = self.cf_of(m);
        let name = cf.class_name(index).map_err(|e| Exc::Internal(e.0))?;
        self.class_of_desc_or_name(&name)
    }

    fn resolve_static(&mut self, m: MethodId, index: u16) -> Res<(ClassId, u32, bool)> {
        let cf = self.cf_of(m);
        let (class, name, desc, _) = cf.member_ref(index).map_err(|e| Exc::Internal(e.0))?;
        let c = self.load(&class)?;
        let nd = self.nd(&name, &desc);
        match self.resolve_field(c, nd) {
            Some((owner, f, true)) => Ok((owner, f.slot as u32, matches!(&*desc, "J" | "D"))),
            Some(_) => {
                let msg = format!("Expected static field {}.{name}", class.replace('/', "."));
                Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&msg)))
            }
            None => {
                let msg = format!("Class {} does not have member field '{} {name}'", class.replace('/', "."), super::class::desc_to_java(&desc));
                Err(self.throw_new("java/lang/NoSuchFieldError", Some(&msg)))
            }
        }
    }

    fn resolve_instance_field(&mut self, m: MethodId, index: u16) -> Res<(u32, bool)> {
        let cf = self.cf_of(m);
        let (class, name, desc, _) = cf.member_ref(index).map_err(|e| Exc::Internal(e.0))?;
        let c = self.load(&class)?;
        let nd = self.nd(&name, &desc);
        match self.resolve_field(c, nd) {
            Some((_, f, false)) => Ok((f.slot as u32, matches!(&*desc, "J" | "D"))),
            Some(_) => {
                let msg = format!("Expected non-static field {}.{name}", class.replace('/', "."));
                Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&msg)))
            }
            None => {
                let msg = format!("Class {} does not have member field '{} {name}'", class.replace('/', "."), super::class::desc_to_java(&desc));
                Err(self.throw_new("java/lang/NoSuchFieldError", Some(&msg)))
            }
        }
    }

    fn no_such_method(&mut self, class: &str, name: &str, desc: &str) -> Exc {
        let msg = format!("'{}'", self.pretty_method(name, desc).replacen(' ', &format!(" {}.", class.replace('/', ".")), 1));
        self.throw_new("java/lang/NoSuchMethodError", Some(&msg))
    }

    /// A virtual or interface call: what to select by, its argument slots, and
    /// the target itself when there is nothing to select (a private or final method).
    fn resolve_virtual(&mut self, m: MethodId, index: u16) -> Res<(NameDesc, u16, Option<MethodId>)> {
        let cf = self.cf_of(m);
        let (class, name, desc, _) = cf.member_ref(index).map_err(|e| Exc::Internal(e.0))?;
        let c = self.class_of_desc_or_name(&class)?;
        let nd = self.nd(&name, &desc);
        if super::jdk::invoke::is_polymorphic(self, c, &name) {
            // MethodHandle.invoke and invokeExact take any descriptor: each one
            // gets a native method of its own that calls the handle.
            let target = super::jdk::invoke::polymorphic_target(self, c, &name, &desc)?;
            return Ok((nd, 0, Some(target)));
        }
        let Some(target) = self.resolve_method(c, nd) else {
            // The JDK's native classes do not list every abstract method their
            // interfaces declare (`IntStream.filter`...): select on the
            // receiver at the call, where the implementing class has it.
            if matches!(self.class(c).kind, super::ClassKind::Native(_)) {
                let Some((params, _)) = super::classfile::parse_method_desc(&desc) else {
                    return Err(self.no_such_method(&class, &name, &desc));
                };
                let slots = super::classfile::arg_slots(&params) as u16;
                return Ok((nd, slots, None));
            }
            return Err(self.no_such_method(&class, &name, &desc));
        };
        if self.method(target).is_static() {
            let msg = format!("Expecting non-static method '{}'", self.pretty_method(&name, &desc));
            return Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&msg)));
        }
        let slots = self.method(target).arg_slots;
        let private = self.method(target).access & super::class::ACC_PRIVATE != 0;
        Ok((nd, slots, private.then_some(target)))
    }

    fn resolve_special(&mut self, m: MethodId, index: u16) -> Res<MethodId> {
        let cf = self.cf_of(m);
        let (class, name, desc, _) = cf.member_ref(index).map_err(|e| Exc::Internal(e.0))?;
        let c = self.class_of_desc_or_name(&class)?;
        let nd = self.nd(&name, &desc);
        let Some(target) = self.resolve_method(c, nd) else {
            return Err(self.no_such_method(&class, &name, &desc));
        };
        // `super.m()` from a class whose superclass is the reference's class
        // selects from the current class's superclass (ACC_SUPER semantics).
        let current = self.methods[m as usize].class;
        if &*name != "<init>" && !self.class(c).is_interface() {
            let cur_super = self.class(current).super_class;
            if let Some(sup) = cur_super
                && c != current
                && self.is_subclass(sup, c)
            {
                let mut k = Some(sup);
                while let Some(id) = k {
                    if let Some(&found) = self.class(id).methods.get(&nd) {
                        return Ok(found);
                    }
                    k = self.class(id).super_class;
                }
            }
        }
        if self.method(target).is_abstract() {
            let msg = format!("'{}'", self.pretty_method(&name, &desc));
            return Err(self.throw_new("java/lang/AbstractMethodError", Some(&msg)));
        }
        Ok(target)
    }

    fn resolve_static_method(&mut self, m: MethodId, index: u16) -> Res<MethodId> {
        let cf = self.cf_of(m);
        let (class, name, desc, _) = cf.member_ref(index).map_err(|e| Exc::Internal(e.0))?;
        let c = self.class_of_desc_or_name(&class)?;
        let nd = self.nd(&name, &desc);
        let Some(target) = self.resolve_method(c, nd) else {
            return Err(self.no_such_method(&class, &name, &desc));
        };
        if !self.method(target).is_static() {
            let msg = format!("Expected static method '{}'", self.pretty_method(&name, &desc));
            return Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&msg)));
        }
        Ok(target)
    }

    // ---------------------------------------------------------------- exceptions

    /// A new throwable of `class` with `message`, its stack trace filled from
    /// the current frames, as `Exc::Throw`.
    pub fn throw_new(&mut self, class: &str, message: Option<&str>) -> Exc {
        match self.make_throwable(class, message) {
            Ok(r) => Exc::Throw(r),
            Err(e) => e,
        }
    }

    pub fn make_throwable(&mut self, class: &str, message: Option<&str>) -> Res<ObjId> {
        let c = match self.load(class) {
            Ok(c) => c,
            Err(_) => return Err(Exc::Internal(format!("{class}: {}", message.unwrap_or("")))),
        };
        let _ = self.ensure_init(c);
        let r = self.new_instance(c)?;
        let msg = match message {
            Some(m) => self.new_string(m)?,
            None => 0,
        };
        let frames = self.stack_frames();
        if let Some(o) = self.obj_mut(r) {
            o.data = Data::Throwable(Box::new(ThrowableData {
                message: msg,
                cause: None,
                frames,
                trace: 0,
                suppressed: Vec::new(),
            }));
        }
        Ok(r)
    }

    /// The current thread's frames as `(method, instruction)`, innermost first.
    pub fn stack_frames(&self) -> Vec<(MethodId, u32)> {
        self.frames
            .iter()
            .rev()
            .map(|f| (f.method, f.pc.saturating_sub(1)))
            .collect()
    }

    pub fn npe(&mut self, message: Option<&str>) -> Exc {
        self.throw_new("java/lang/NullPointerException", message)
    }

    pub fn out_of_memory(&mut self, message: &str) -> Exc {
        self.throw_new("java/lang/OutOfMemoryError", Some(message))
    }

    /// HotSpot's message for a failed cast.
    pub fn class_cast_message(&self, r: ObjId, target: ClassId) -> String {
        let from = self.class_of(r).unwrap_or(0);
        let module = |c: ClassId| -> String {
            let mut c = c;
            while let ClassKind::Array { component, .. } = self.class(c).kind {
                c = component;
            }
            if matches!(self.class(c).kind, ClassKind::Native(_) | ClassKind::Primitive(_)) {
                "module java.base of loader 'bootstrap'".to_string()
            } else {
                "unnamed module of loader 'app'".to_string()
            }
        };
        let (a, b) = (self.class(from).java_name(), self.class(target).java_name());
        let (ma, mb) = (module(from), module(target));
        if ma == mb {
            format!("class {a} cannot be cast to class {b} ({a} and {b} are in {ma})")
        } else {
            format!("class {a} cannot be cast to class {b} ({a} is in {ma}; {b} is in {mb})")
        }
    }

    /// JEP 358's message for the null at instruction `at` of method `m`.
    fn helpful_npe(&self, m: MethodId, at: usize) -> Option<String> {
        super::jdk::lang::helpful_npe(self, m, at)
    }

    // ---------------------------------------------------------------- monitors

    pub(crate) fn monitor_enter(&mut self, r: ObjId) -> Res<()> {
        let thread = self.current_thread;
        let entry = self.jdk.monitors.entry(r).or_insert((thread, 0));
        if entry.1 == 0 || entry.0 == thread {
            entry.0 = thread;
            entry.1 += 1;
            return Ok(());
        }
        super::jdk::thread::block_on_monitor(self, r)
    }

    pub(crate) fn monitor_exit(&mut self, r: ObjId) -> Res<()> {
        let thread = self.current_thread;
        match self.jdk.monitors.get_mut(&r) {
            Some(entry) if entry.0 == thread && entry.1 > 0 => {
                entry.1 -= 1;
                if entry.1 == 0 {
                    self.jdk.monitors.remove(&r);
                }
                Ok(())
            }
            _ => Err(self.throw_new("java/lang/IllegalMonitorStateException", None)),
        }
    }

    // ---------------------------------------------------------------- main

    /// Runs `main` as the launcher does, then every thread still running.
    pub fn launch_main(&mut self, class: &str, args: &[String]) -> MainOutcome {
        let result = (|| -> Res<()> {
            let c = match self.try_load(class)? {
                Some(c) => c,
                None => {
                    let msg = format!("Could not find or load main class {}\nCaused by: java.lang.ClassNotFoundException: {}", class.replace('/', "."), class.replace('/', "."));
                    self.stderr.extend_from_slice(format!("Error: {msg}\n").as_bytes());
                    return Err(Exc::Exit(1));
                }
            };
            let string_array = self.array_class("[Ljava/lang/String;")?;
            let argv = self.new_array(string_array, args.len() as i32)?;
            for (i, a) in args.iter().enumerate() {
                let s = self.new_string(a)?;
                if let Some(Array::L(v)) = self.array_mut(argv) {
                    v[i] = s;
                }
            }
            let main_args = self.find_method(c, "main", "([Ljava/lang/String;)V");
            let main_none = self.find_method(c, "main", "()V");
            let (target, takes_args) = match (main_args, main_none) {
                (Some(m), _) => (m, true),
                (None, Some(m)) => (m, false),
                (None, None) => {
                    let msg = format!("Error: Main method not found in class {}, please define the main method as:\n   public static void main(String[] args)\nor a JavaFX application class must extend javafx.application.Application\n", self.class(c).java_name());
                    self.stderr.extend_from_slice(msg.as_bytes());
                    return Err(Exc::Exit(1));
                }
            };
            if let Err(e) = self.ensure_init(c) {
                if let Exc::Throw(ex) = e {
                    let text = self.throwable_text(ex);
                    let msg = format!("Exception in thread \"main\" {text}\n");
                    self.stderr.extend_from_slice(msg.as_bytes());
                    return Err(Exc::Exit(1));
                }
                return Err(e);
            }
            let mut call_args = Vec::new();
            if !self.method(target).is_static() {
                let this = self.new_instance(c)?;
                if let Some(ctor) = self.find_method(c, "<init>", "()V") {
                    self.invoke(ctor, &[Value::Ref(this)])?;
                }
                call_args.push(Value::Ref(this));
            }
            if takes_args {
                call_args.push(Value::Ref(argv));
            }
            self.jdk.main_started = true;
            self.invoke(target, &call_args)?;
            super::jdk::thread::run_remaining(self)?;
            Ok(())
        })();
        match result {
            Ok(()) => MainOutcome::Returned,
            Err(Exc::Exit(code)) => MainOutcome::Exited(code),
            Err(Exc::Throw(ex)) => {
                let class = self.class_of(ex).map(|c| self.class(c).java_name()).unwrap_or_default();
                let text = self.throwable_text(ex);
                let trace = super::jdk::lang::trace_lines(self, ex);
                let mut msg = format!("Exception in thread \"main\" {text}\n");
                msg.push_str(&trace);
                self.stderr.extend_from_slice(msg.as_bytes());
                // Threads the program started still run to their end.
                if let Err(e) = super::jdk::thread::run_remaining(self) {
                    return MainOutcome::Stopped(format!("{e:?}"));
                }
                MainOutcome::Uncaught { class, text }
            }
            Err(Exc::Unsupported(what)) => MainOutcome::Stopped(format!("unsupported: {what}")),
            Err(Exc::Limit(what)) => MainOutcome::Stopped(format!("limit: {what}")),
            Err(Exc::Internal(what)) => MainOutcome::Stopped(format!("internal: {what}")),
        }
    }

    /// A throwable's `toString()`, falling back to its class name.
    pub fn throwable_text(&mut self, ex: ObjId) -> String {
        let saved = (self.stack.len(), self.frames.len());
        let text = self
            .invoke_virtual(ex, "toString", "()Ljava/lang/String;", &[])
            .ok()
            .and_then(|v| self.string(v.r()));
        self.stack.truncate(saved.0);
        self.frames.truncate(saved.1);
        text.unwrap_or_else(|| self.class_of(ex).map(|c| self.class(c).java_name()).unwrap_or_default())
    }
}

/// Decodes a method's bytecode into instructions, checking every branch
/// target, local index and constant-pool index's range as it goes.
fn decode_code(code: &Arc<Code>, cf: &Arc<ClassFile>) -> Result<Decoded, String> {
    let bytes = &code.code;
    let max_locals = code.max_locals as usize;
    // Instruction starts, and the index each byte offset decodes to.
    let mut starts = Vec::new();
    let mut index_of = vec![u32::MAX; bytes.len() + 1];
    let mut at = 0usize;
    while at < bytes.len() {
        index_of[at] = starts.len() as u32;
        starts.push(at);
        at += instruction_length(bytes, at).ok_or_else(|| format!("bad instruction at {at}"))?;
    }
    if at != bytes.len() {
        return Err("an instruction overruns the code".into());
    }
    index_of[bytes.len()] = starts.len() as u32;
    let target = |from: usize, offset: i32| -> Result<u32, String> {
        let t = from as i64 + offset as i64;
        if t < 0 || t as usize >= bytes.len() || index_of[t as usize] == u32::MAX {
            return Err(format!("bad branch target {t}"));
        }
        Ok(index_of[t as usize])
    };
    let u2 = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
    let i2 = |at: usize| i16::from_be_bytes([bytes[at], bytes[at + 1]]);
    let i4 = |at: usize| i32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let local = |i: usize, wide: bool| -> Result<u16, String> {
        if i + wide as usize >= max_locals {
            return Err(format!("local {i} out of range"));
        }
        Ok(i as u16)
    };
    let cp = |i: u16| -> Result<u16, String> {
        if (i as usize) < cf.pool.len() && i != 0 {
            Ok(i)
        } else {
            Err(format!("constant pool index {i} out of range"))
        }
    };
    let mut ops = Vec::with_capacity(starts.len());
    let mut switches = Vec::new();
    for &at in &starts {
        let opcode = bytes[at];
        let op = match opcode {
            0x00 => Op::Nop,
            0x01 => Op::Push(Value::NULL),
            0x02..=0x08 => Op::Push(Value::Int(opcode as i32 - 3)),
            0x09 | 0x0a => Op::Push2(Value::Long(opcode as i64 - 9)),
            0x0b..=0x0d => Op::Push(Value::Float((opcode - 0x0b) as f32)),
            0x0e | 0x0f => Op::Push2(Value::Double((opcode - 0x0e) as f64)),
            0x10 => Op::Push(Value::Int(bytes[at + 1] as i8 as i32)),
            0x11 => Op::Push(Value::Int(i2(at + 1) as i32)),
            0x12 => match cf.get(bytes[at + 1] as u16).map_err(|e| e.0)? {
                Const::Int(v) => Op::Push(Value::Int(*v)),
                Const::Float(v) => Op::Push(Value::Float(*v)),
                _ => Op::Ldc(cp(bytes[at + 1] as u16)?),
            },
            0x13 | 0x14 => match cf.get(u2(at + 1)).map_err(|e| e.0)? {
                Const::Int(v) => Op::Push(Value::Int(*v)),
                Const::Float(v) => Op::Push(Value::Float(*v)),
                Const::Long(v) => Op::Push2(Value::Long(*v)),
                Const::Double(v) => Op::Push2(Value::Double(*v)),
                _ => Op::Ldc(cp(u2(at + 1))?),
            },
            0x15 | 0x17 | 0x19 => Op::Load(local(bytes[at + 1] as usize, false)?),
            0x16 | 0x18 => Op::Load2(local(bytes[at + 1] as usize, true)?),
            0x1a..=0x2d => {
                let n = ((opcode - 0x1a) % 4) as usize;
                let kind = (opcode - 0x1a) / 4;
                if kind == 1 || kind == 3 {
                    Op::Load2(local(n, true)?)
                } else {
                    Op::Load(local(n, false)?)
                }
            }
            0x2e => Op::ALoad(b'I'),
            0x2f => Op::ALoad(b'J'),
            0x30 => Op::ALoad(b'F'),
            0x31 => Op::ALoad(b'D'),
            0x32 => Op::ALoad(b'L'),
            0x33 => Op::ALoad(b'B'),
            0x34 => Op::ALoad(b'C'),
            0x35 => Op::ALoad(b'S'),
            0x36 | 0x38 | 0x3a => Op::Store(local(bytes[at + 1] as usize, false)?),
            0x37 | 0x39 => Op::Store2(local(bytes[at + 1] as usize, true)?),
            0x3b..=0x4e => {
                let n = ((opcode - 0x3b) % 4) as usize;
                let kind = (opcode - 0x3b) / 4;
                if kind == 1 || kind == 3 {
                    Op::Store2(local(n, true)?)
                } else {
                    Op::Store(local(n, false)?)
                }
            }
            0x4f => Op::AStore(b'I'),
            0x50 => Op::AStore(b'J'),
            0x51 => Op::AStore(b'F'),
            0x52 => Op::AStore(b'D'),
            0x53 => Op::AStore(b'L'),
            0x54 => Op::AStore(b'B'),
            0x55 => Op::AStore(b'C'),
            0x56 => Op::AStore(b'S'),
            0x57 => Op::Pop,
            0x58 => Op::Pop2,
            0x59 => Op::Dup,
            0x5a => Op::DupX1,
            0x5b => Op::DupX2,
            0x5c => Op::Dup2,
            0x5d => Op::Dup2X1,
            0x5e => Op::Dup2X2,
            0x5f => Op::Swap,
            0x60..=0x73 => {
                let o = (opcode - 0x60) / 4;
                match (opcode - 0x60) % 4 {
                    0 => Op::IBin(o),
                    1 => Op::LBin(o),
                    2 => Op::FBin(o),
                    _ => Op::DBin(o),
                }
            }
            0x74 => Op::INeg,
            0x75 => Op::LNeg,
            0x76 => Op::FNeg,
            0x77 => Op::DNeg,
            0x78 => Op::IBin(5),
            0x79 => Op::LBin(5),
            0x7a => Op::IBin(6),
            0x7b => Op::LBin(6),
            0x7c => Op::IBin(7),
            0x7d => Op::LBin(7),
            0x7e => Op::IBin(8),
            0x7f => Op::LBin(8),
            0x80 => Op::IBin(9),
            0x81 => Op::LBin(9),
            0x82 => Op::IBin(10),
            0x83 => Op::LBin(10),
            0x84 => Op::IInc(local(bytes[at + 1] as usize, false)?, bytes[at + 2] as i8 as i16),
            0x85..=0x93 => Op::Conv(opcode - 0x85),
            0x94 => Op::LCmp,
            0x95 => Op::FCmp(-1),
            0x96 => Op::FCmp(1),
            0x97 => Op::DCmp(-1),
            0x98 => Op::DCmp(1),
            0x99..=0x9e => Op::IfZ(cmp(opcode - 0x99), target(at, i2(at + 1) as i32)?),
            0x9f..=0xa4 => Op::IfICmp(cmp(opcode - 0x9f), target(at, i2(at + 1) as i32)?),
            0xa5 => Op::IfACmp(true, target(at, i2(at + 1) as i32)?),
            0xa6 => Op::IfACmp(false, target(at, i2(at + 1) as i32)?),
            0xa7 => Op::Goto(target(at, i2(at + 1) as i32)?),
            0xa8 => Op::Jsr(target(at, i2(at + 1) as i32)?),
            0xa9 => Op::Ret(local(bytes[at + 1] as usize, false)?),
            0xaa | 0xab => {
                let base = (at + 4) & !3;
                let default = target(at, i4(base))?;
                let mut keys = Vec::new();
                let mut targets = Vec::new();
                if opcode == 0xaa {
                    let (low, high) = (i4(base + 4), i4(base + 8));
                    for (n, key) in (low..=high).enumerate() {
                        keys.push(key);
                        targets.push(target(at, i4(base + 12 + 4 * n))?);
                    }
                } else {
                    let n = i4(base + 4).max(0) as usize;
                    let mut pairs = Vec::with_capacity(n);
                    for k in 0..n {
                        pairs.push((i4(base + 8 + 8 * k), target(at, i4(base + 12 + 8 * k))?));
                    }
                    pairs.sort_by_key(|p| p.0);
                    for (k, t) in pairs {
                        keys.push(k);
                        targets.push(t);
                    }
                }
                switches.push(Switch { default, keys, targets });
                Op::Switch(switches.len() as u32 - 1)
            }
            0xac | 0xae | 0xb0 => Op::ReturnValue,
            0xad | 0xaf => Op::ReturnWide,
            0xb1 => Op::Return,
            0xb2 => Op::GetStatic(cp(u2(at + 1))?),
            0xb3 => Op::PutStatic(cp(u2(at + 1))?),
            0xb4 => Op::GetField(cp(u2(at + 1))?),
            0xb5 => Op::PutField(cp(u2(at + 1))?),
            0xb6 => Op::InvokeVirtual(cp(u2(at + 1))?),
            0xb7 => Op::InvokeSpecial(cp(u2(at + 1))?),
            0xb8 => Op::InvokeStatic(cp(u2(at + 1))?),
            0xb9 => Op::InvokeInterface(cp(u2(at + 1))?),
            0xba => Op::InvokeDynamic(cp(u2(at + 1))?),
            0xbb => Op::New(cp(u2(at + 1))?),
            0xbc => {
                let t = bytes[at + 1];
                let elem = match t {
                    4 => b'Z',
                    5 => b'C',
                    6 => b'F',
                    7 => b'D',
                    8 => b'B',
                    9 => b'S',
                    10 => b'I',
                    11 => b'J',
                    _ => return Err(format!("bad newarray type {t}")),
                };
                Op::NewArray(elem)
            }
            0xbd => Op::ANewArray(cp(u2(at + 1))?),
            0xbe => Op::ArrayLength,
            0xbf => Op::AThrow,
            0xc0 => Op::CheckCast(cp(u2(at + 1))?),
            0xc1 => Op::InstanceOf(cp(u2(at + 1))?),
            0xc2 => Op::MonitorEnter,
            0xc3 => Op::MonitorExit,
            0xc4 => {
                let inner = bytes[at + 1];
                let i = u2(at + 2) as usize;
                match inner {
                    0x15 | 0x17 | 0x19 => Op::Load(local(i, false)?),
                    0x16 | 0x18 => Op::Load2(local(i, true)?),
                    0x36 | 0x38 | 0x3a => Op::Store(local(i, false)?),
                    0x37 | 0x39 => Op::Store2(local(i, true)?),
                    0x84 => Op::IInc(local(i, false)?, i2(at + 4)),
                    0xa9 => Op::Ret(local(i, false)?),
                    _ => return Err(format!("bad wide opcode {inner}")),
                }
            }
            0xc5 => Op::MultiANewArray(cp(u2(at + 1))?, bytes[at + 3]),
            0xc6 => Op::IfNull(true, target(at, i2(at + 1) as i32)?),
            0xc7 => Op::IfNull(false, target(at, i2(at + 1) as i32)?),
            0xc8 => Op::Goto(target(at, i4(at + 1))?),
            0xc9 => Op::Jsr(target(at, i4(at + 1))?),
            other => return Err(format!("bad opcode {other:#x}")),
        };
        ops.push(op);
    }
    let handlers = code
        .handlers
        .iter()
        .map(|h| {
            Ok(HandlerOp {
                start: *index_of.get(h.start as usize).filter(|&&i| i != u32::MAX).ok_or("bad handler range")?,
                end: *index_of.get(h.end as usize).filter(|&&i| i != u32::MAX).ok_or("bad handler range")?,
                handler: *index_of.get(h.handler as usize).filter(|&&i| i != u32::MAX).ok_or("bad handler")?,
                catch_type: h.catch_type,
            })
        })
        .collect::<Result<Vec<_>, &str>>()?;
    // The operand stack of a frame may not exceed max_stack: three constants
    // pushed with room for one are refused, as a verifier would.
    check_stack_depth(&ops, code.max_stack as usize)?;
    Ok(Decoded {
        ops,
        pcs: starts.iter().map(|&s| s as u32).collect(),
        switches,
        handlers,
        max_locals: code.max_locals,
        max_stack: code.max_stack,
        code: code.clone(),
        cf: cf.clone(),
    })
}

/// A straight-line check of the operand stack's depth: the first block's
/// constants and loads against `max_stack` (the full verifier is a later step).
fn check_stack_depth(ops: &[Op], max_stack: usize) -> Result<(), String> {
    let mut depth = 0usize;
    for op in ops {
        depth += match op {
            Op::Push(_) | Op::Load(_) | Op::Dup | Op::LdcRef(_) => 1,
            Op::Push2(_) | Op::Load2(_) | Op::Dup2 => 2,
            Op::Ldc(_) => 1,
            _ => return Ok(()),
        };
        if depth > max_stack {
            return Err("Operand stack overflow".into());
        }
    }
    Ok(())
}

fn cmp(n: u8) -> Cmp {
    match n {
        0 => Cmp::Eq,
        1 => Cmp::Ne,
        2 => Cmp::Lt,
        3 => Cmp::Ge,
        4 => Cmp::Gt,
        _ => Cmp::Le,
    }
}

/// The length of the instruction at `at`, or `None` if it overruns the code
/// or is not an opcode.
fn instruction_length(code: &[u8], at: usize) -> Option<usize> {
    let op = *code.get(at)?;
    let len = match op {
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
        0xc5 => 4,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        0xc4 => {
            if *code.get(at + 1)? == 0x84 {
                6
            } else {
                4
            }
        }
        0xaa => {
            let base = (at + 4) & !3;
            let low = i32::from_be_bytes(code.get(base + 4..base + 8)?.try_into().ok()?);
            let high = i32::from_be_bytes(code.get(base + 8..base + 12)?.try_into().ok()?);
            if high < low || (high as i64 - low as i64) > 65536 {
                return None;
            }
            base + 12 + 4 * (high - low + 1) as usize - at
        }
        0xab => {
            let base = (at + 4) & !3;
            let n = i32::from_be_bytes(code.get(base + 4..base + 8)?.try_into().ok()?);
            if !(0..=65536).contains(&n) {
                return None;
            }
            base + 8 + 8 * n as usize - at
        }
        0x00..=0x0f | 0x1a..=0x35 | 0x3b..=0x83 | 0x85..=0x98 | 0xac..=0xb1 | 0xbe | 0xbf | 0xc2 | 0xc3 => 1,
        _ => return None,
    };
    (at + len <= code.len()).then_some(len)
}

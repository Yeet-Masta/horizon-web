//! `java.lang.invoke`: method handles, method types, lookups, var handles,
//! and the constant bootstraps javac emits for pattern switches.
use super::{A, FINAL, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, this, void};
use crate::natives;
use crate::vm::class::{ACC_FINAL, ACC_NATIVE, ACC_PUBLIC, ACC_STATIC, ACC_VARARGS, ClassKind, MethodBody};
use crate::vm::classfile::{self, ClassFile};
use crate::vm::indy::{HandleRef, handle_ref};
use crate::vm::interp::Vm;
use crate::vm::{Array, ClassId, Data, Exc, MethodId, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

/// What a method handle calls.
#[derive(Debug, Clone)]
pub enum Target {
    Static(MethodId),
    /// Selected on the receiver (`findVirtual`, an interface method).
    Virtual(MethodId),
    /// Exactly this method (`findSpecial`, a private method).
    Special(MethodId),
    Constructor(MethodId, ClassId),
    Getter { owner: ClassId, slot: usize, is_static: bool },
    Setter { owner: ClassId, slot: usize, is_static: bool },
    Constant(Value),
    Identity,
    ArrayGetter,
    ArraySetter,
    /// A `VarHandle` on a field, or on an array's elements (`owner` 0).
    VarField { owner: ClassId, slot: usize, is_static: bool },
    VarArray,
}

#[derive(Debug, Clone)]
pub struct Handle {
    pub target: Target,
    /// Arguments inserted (`bindTo`, `insertArguments`): position, value.
    pub bound: Vec<(usize, Value)>,
    /// The handle's type (after its bound arguments).
    pub desc: String,
    /// The type of the value a `VarHandle` holds.
    pub var_desc: String,
}

impl Handle {
    pub fn refs(&self, f: &mut dyn FnMut(Value)) {
        self.bound.iter().for_each(|(_, v)| f(*v));
        if let Target::Constant(v) = self.target {
            f(v);
        }
    }
}

fn handle(vm: &Vm, r: ObjId) -> Option<&Handle> {
    match state(vm, r) {
        Some(State::Handle(h)) => Some(h),
        _ => None,
    }
}

fn new_handle(vm: &mut Vm, h: Handle) -> Res<ObjId> {
    let class = if matches!(h.target, Target::VarField { .. } | Target::VarArray) { "java/lang/invoke/VarHandle" } else { "java/lang/invoke/MethodHandle" };
    let c = vm.load(class)?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Handle(Box::new(h))));
    }
    Ok(r)
}

// ------------------------------------------------------------------ method types

/// The descriptor a `MethodType` object stands for.
pub fn method_type_desc(vm: &Vm, r: ObjId) -> Option<String> {
    match state(vm, r) {
        Some(State::Generic(v)) if !v.is_empty() => vm.string(v[0].r()),
        _ => None,
    }
}

pub fn method_type_of(vm: &mut Vm, desc: &str) -> Res<ObjId> {
    let c = vm.load("java/lang/invoke/MethodType")?;
    let r = vm.new_instance(c)?;
    let d = vm.intern(desc)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(d)])));
    }
    Ok(r)
}

fn class_desc(vm: &mut Vm, mirror: ObjId) -> Res<String> {
    match vm.mirror_class(mirror) {
        Some(c) => Ok(vm.descriptor_of(c)),
        None => Err(vm.npe(None)),
    }
}

fn mirror_of_desc(vm: &mut Vm, desc: &str) -> Res<ObjId> {
    let c = vm.class_of_desc(desc)?;
    vm.mirror(c)
}

fn method_type_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "methodType" => {
            // (rtype), (rtype, ptype), (rtype, ptype...), (rtype, Class[]), (rtype, List), (rtype, MethodType)
            let ret = class_desc(vm, arg(a, 0).r())?;
            let mut params = Vec::new();
            for v in &a[1..] {
                let r = v.r();
                if r == 0 {
                    return Err(vm.npe(None));
                }
                if vm.mirror_class(r).is_some() {
                    params.push(class_desc(vm, r)?);
                } else if vm.array(r).is_some() {
                    for m in vm.ref_elements(r) {
                        params.push(class_desc(vm, m)?);
                    }
                } else if let Some(d) = method_type_desc(vm, r) {
                    let (p, _) = classfile::parse_method_desc(&d).unwrap_or_default();
                    params.extend(p.iter().map(|s| s.to_string()));
                } else {
                    for m in super::util::iterate(vm, r)? {
                        params.push(class_desc(vm, m.r())?);
                    }
                }
            }
            let desc = format!("({}){ret}", params.concat());
            ret_ref(method_type_of(vm, &desc)?)
        }
        "generic" => {
            let n = arg(a, 0).i().max(0) as usize;
            ret_ref(method_type_of(vm, &format!("({})Ljava/lang/Object;", "Ljava/lang/Object;".repeat(n)))?)
        }
        "fromDescriptor" => {
            let d = vm.string_nn(arg(a, 0).r())?;
            if classfile::parse_method_desc(&d).is_none() {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&format!("not a method descriptor: {d}"))));
            }
            ret_ref(method_type_of(vm, &d)?)
        }
        _ => {
            let t = this(vm, a)?;
            let desc = method_type_desc(vm, t).unwrap_or_default();
            let (params, ret) = classfile::parse_method_desc(&desc).map(|(p, r)| (p.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.to_string())).unwrap_or_default();
            match op {
                "parameterCount" => Ok(Value::Int(params.len() as i32)),
                "parameterType" => {
                    let i = arg(a, 1).i();
                    match params.get(i as usize).filter(|_| i >= 0) {
                        Some(p) => ret_ref(mirror_of_desc(vm, &p.clone())?),
                        None => Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&i.to_string()))),
                    }
                }
                "returnType" => ret_ref(mirror_of_desc(vm, &ret)?),
                "parameterArray" | "parameterList" => {
                    let mut mirrors = Vec::new();
                    for p in &params {
                        mirrors.push(mirror_of_desc(vm, p)?);
                    }
                    if op == "parameterArray" {
                        ret_ref(vm.new_ref_array("Ljava/lang/Class;", &mirrors)?)
                    } else {
                        let items = mirrors.into_iter().map(Value::Ref).collect();
                        super::util::immutable_list(vm, items)
                    }
                }
                "changeReturnType" => {
                    let r = class_desc(vm, arg(a, 1).r())?;
                    ret_ref(method_type_of(vm, &format!("({}){r}", params.concat()))?)
                }
                "changeParameterType" => {
                    let mut p = params.clone();
                    let i = arg(a, 1).i() as usize;
                    if i < p.len() {
                        p[i] = class_desc(vm, arg(a, 2).r())?;
                    }
                    ret_ref(method_type_of(vm, &format!("({}){ret}", p.concat()))?)
                }
                "appendParameterTypes" | "insertParameterTypes" | "dropParameterTypes" => {
                    let mut p = params.clone();
                    match op {
                        "dropParameterTypes" => {
                            let (s, e) = (arg(a, 1).i().max(0) as usize, arg(a, 2).i().max(0) as usize);
                            if s <= e && e <= p.len() {
                                p.drain(s..e);
                            }
                        }
                        _ => {
                            let (at, from) = if op == "insertParameterTypes" { (arg(a, 1).i().max(0) as usize, 2) } else { (p.len(), 1) };
                            let mut extra = Vec::new();
                            for v in &a[from..] {
                                if vm.array(v.r()).is_some() {
                                    for m in vm.ref_elements(v.r()) {
                                        extra.push(class_desc(vm, m)?);
                                    }
                                } else {
                                    extra.push(class_desc(vm, v.r())?);
                                }
                            }
                            p.splice(at.min(p.len())..at.min(p.len()), extra);
                        }
                    }
                    ret_ref(method_type_of(vm, &format!("({}){ret}", p.concat()))?)
                }
                "descriptorString" | "toMethodDescriptorString" => ret_str(vm, &desc),
                "toString" => {
                    let mut names = Vec::new();
                    for p in &params {
                        let c = vm.class_of_desc(p)?;
                        names.push(super::lang::simple_name(vm, c));
                    }
                    let rc = vm.class_of_desc(&ret)?;
                    let rn = super::lang::simple_name(vm, rc);
                    ret_str(vm, &format!("({}){rn}", names.join(",")))
                }
                "equals" => {
                    let o = arg(a, 1).r();
                    ret_bool(method_type_desc(vm, o).as_deref() == Some(&desc))
                }
                "hashCode" => {
                    let u: Vec<u16> = desc.encode_utf16().collect();
                    Ok(Value::Int(super::string::java_hash(&u)))
                }
                "wrap" | "unwrap" | "erase" | "generic" => ret_ref(t),
                _ => Err(Exc::Unsupported(format!("MethodType.{op}"))),
            }
        }
    }
}

// ------------------------------------------------------------------ lookups

fn new_lookup(vm: &mut Vm, class: ClassId) -> Res<ObjId> {
    let c = vm.load("java/lang/invoke/MethodHandles$Lookup")?;
    let r = vm.new_instance(c)?;
    let m = vm.mirror(class)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(m)])));
    }
    Ok(r)
}

/// The class of the bytecode method that called the running native.
pub fn caller_class(vm: &Vm) -> ClassId {
    vm.frames.last().map_or(vm.known.object, |f| vm.method(f.method).class)
}

fn no_such(vm: &mut Vm, class: &str, what: String) -> Exc {
    vm.throw_new(class, Some(&what))
}

fn find(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let refc = vm.mirror_class(arg(a, 1).r()).ok_or_else(|| Exc::Internal("lookup without a class".into()))?;
    let cname = vm.class(refc).java_name();
    match op {
        "findStatic" | "findVirtual" | "findSpecial" => {
            let name = vm.string_nn(arg(a, 2).r())?;
            let desc = method_type_desc(vm, arg(a, 3).r()).unwrap_or_default();
            let nd = vm.nd(&name, &desc);
            let kind = match op {
                "findStatic" => "invokeStatic",
                "findSpecial" => "invokeSpecial",
                _ if vm.class(refc).is_interface() => "invokeInterface",
                _ => "invokeVirtual",
            };
            let Some(m) = vm.resolve_method(refc, nd) else {
                let tt = type_text(vm, &desc)?;
                return Err(no_such(vm, "java/lang/NoSuchMethodException", format!("no such method: {cname}.{name}{tt}/{kind}")));
            };
            let is_static = vm.method(m).is_static();
            if is_static != (op == "findStatic") {
                let msg = format!("{} method: {cname}.{name}{}/{kind}", if is_static { "expected instance not static" } else { "expected static not instance" }, type_text(vm, &desc)?);
                return Err(vm.throw_new("java/lang/IllegalAccessException", Some(&msg)));
            }
            let private = vm.method(m).access & crate::vm::class::ACC_PRIVATE != 0;
            let (target, hdesc) = match op {
                "findStatic" => (Target::Static(m), desc.clone()),
                "findSpecial" => {
                    // The handle's receiver is the special caller.
                    let caller = vm.mirror_class(arg(a, 4).r()).unwrap_or(refc);
                    (Target::Special(m), receiver_desc(vm, caller, &desc))
                }
                _ if private => (Target::Special(m), receiver_desc(vm, refc, &desc)),
                _ => (Target::Virtual(m), receiver_desc(vm, refc, &desc)),
            };
            ret_ref(new_handle(vm, Handle { target, bound: Vec::new(), desc: hdesc, var_desc: String::new() })?)
        }
        "findConstructor" => {
            let desc = method_type_desc(vm, arg(a, 2).r()).unwrap_or_default();
            let nd = vm.nd("<init>", &desc);
            let found = vm.class(refc).methods.get(&nd).copied();
            let Some(m) = found else {
                let tt = type_text(vm, &desc)?;
                return Err(no_such(vm, "java/lang/NoSuchMethodException", format!("no such constructor: {cname}.<init>{tt}/newInvokeSpecial")));
            };
            let (params, _) = classfile::parse_method_desc(&desc).unwrap_or_default();
            let hdesc = format!("({}){}", params.concat(), vm.descriptor_of(refc));
            ret_ref(new_handle(vm, Handle { target: Target::Constructor(m, refc), bound: Vec::new(), desc: hdesc, var_desc: String::new() })?)
        }
        "findGetter" | "findSetter" | "findStaticGetter" | "findStaticSetter" | "findVarHandle" | "findStaticVarHandle" => {
            let name = vm.string_nn(arg(a, 2).r())?;
            let fdesc = class_desc(vm, arg(a, 3).r())?;
            let nd = vm.nd(&name, &fdesc);
            let want_static = op.contains("Static");
            let Some((owner, f, is_static)) = vm.resolve_field(refc, nd) else {
                let ds = desc_simple(vm, &fdesc)?;
                return Err(no_such(vm, "java/lang/NoSuchFieldException", format!("no such field: {cname}.{name}/{ds}")));
            };
            if is_static != want_static {
                let msg = format!("{} field: {cname}.{name}/{}", if is_static { "expected instance not static" } else { "expected static not instance" }, desc_simple(vm, &fdesc)?);
                return Err(vm.throw_new("java/lang/IllegalAccessException", Some(&msg)));
            }
            let recv = if is_static { String::new() } else { vm.descriptor_of(refc) };
            let (target, desc) = match op {
                "findGetter" | "findStaticGetter" => (Target::Getter { owner, slot: f.slot, is_static }, format!("({recv}){fdesc}")),
                "findSetter" | "findStaticSetter" => (Target::Setter { owner, slot: f.slot, is_static }, format!("({recv}{fdesc})V")),
                _ => (Target::VarField { owner, slot: f.slot, is_static }, format!("({recv}){fdesc}")),
            };
            ret_ref(new_handle(vm, Handle { target, bound: Vec::new(), desc, var_desc: fdesc })?)
        }
        _ => Err(Exc::Unsupported(format!("Lookup.{op}"))),
    }
}

fn receiver_desc(vm: &Vm, refc: ClassId, desc: &str) -> String {
    format!("({}{}", vm.descriptor_of(refc), &desc[1..])
}

fn type_text(vm: &mut Vm, desc: &str) -> Res<String> {
    let t = method_type_of(vm, desc)?;
    let s = method_type_op(vm, &[Value::Ref(t)], "toString")?;
    Ok(vm.string(s.r()).unwrap_or_default())
}

fn desc_simple(vm: &mut Vm, desc: &str) -> Res<String> {
    let c = vm.class_of_desc(desc)?;
    Ok(super::lang::simple_name(vm, c))
}

fn lookup_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let mirror = match state(vm, t) {
        Some(State::Generic(v)) => v[0].r(),
        _ => 0,
    };
    match op {
        "lookupClass" => ret_ref(mirror),
        "in" => {
            let c = vm.mirror_class(arg(a, 1).r()).unwrap_or(vm.known.object);
            ret_ref(new_lookup(vm, c)?)
        }
        "toString" => {
            let c = vm.mirror_class(mirror).unwrap_or(vm.known.object);
            let n = vm.class(c).java_name();
            ret_str(vm, &n)
        }
        "unreflect" | "unreflectSpecial" => {
            let Some(super::reflect::Member::Method(m)) = super::reflect::member(vm, arg(a, 1).r()) else { return Err(vm.npe(None)) };
            let method = vm.method(m).clone();
            let (target, desc) = if method.is_static() {
                (Target::Static(m), method.desc.to_string())
            } else {
                let recv = receiver_desc(vm, method.class, &method.desc);
                (if op == "unreflectSpecial" { Target::Special(m) } else { Target::Virtual(m) }, recv)
            };
            ret_ref(new_handle(vm, Handle { target, bound: Vec::new(), desc, var_desc: String::new() })?)
        }
        "unreflectConstructor" => {
            let Some(super::reflect::Member::Constructor(m)) = super::reflect::member(vm, arg(a, 1).r()) else { return Err(vm.npe(None)) };
            let method = vm.method(m).clone();
            let (params, _) = classfile::parse_method_desc(&method.desc).unwrap_or_default();
            let desc = format!("({}){}", params.concat(), vm.descriptor_of(method.class));
            ret_ref(new_handle(vm, Handle { target: Target::Constructor(m, method.class), bound: Vec::new(), desc, var_desc: String::new() })?)
        }
        "unreflectGetter" | "unreflectSetter" | "unreflectVarHandle" => {
            let Some(super::reflect::Member::Field { owner, slot, is_static, desc, .. }) = super::reflect::member(vm, arg(a, 1).r()) else { return Err(vm.npe(None)) };
            let recv = if is_static { String::new() } else { vm.descriptor_of(owner) };
            let (target, hdesc) = match op {
                "unreflectGetter" => (Target::Getter { owner, slot, is_static }, format!("({recv}){desc}")),
                "unreflectSetter" => (Target::Setter { owner, slot, is_static }, format!("({recv}{desc})V")),
                _ => (Target::VarField { owner, slot, is_static }, format!("({recv}){desc}")),
            };
            ret_ref(new_handle(vm, Handle { target, bound: Vec::new(), desc: hdesc, var_desc: desc.to_string() })?)
        }
        "findClass" => {
            let name = vm.string_nn(arg(a, 1).r())?;
            match vm.try_load(&name.replace('.', "/"))? {
                Some(c) => ret_ref(vm.mirror(c)?),
                None => Err(vm.throw_new("java/lang/ClassNotFoundException", Some(&name))),
            }
        }
        "accessClass" | "ensureInitialized" => {
            if op == "ensureInitialized"
                && let Some(c) = vm.mirror_class(arg(a, 1).r())
            {
                vm.ensure_init(c)?;
            }
            Ok(arg(a, 1))
        }
        "lookupModes" => Ok(Value::Int(0x1F)),
        _ => find(vm, a, op),
    }
}

// ------------------------------------------------------------------ invoking

/// Calls a handle with logical arguments (after its bound ones): converted to
/// its target's types, boxed or unboxed as `asType` would.
pub fn call_handle(vm: &mut Vm, h: ObjId, args: &[Value]) -> Res<Value> {
    let Some(hd) = handle(vm, h).cloned() else { return Err(vm.npe(None)) };
    let mut all: Vec<Value> = args.to_vec();
    for (pos, v) in &hd.bound {
        all.insert((*pos).min(all.len()), *v);
    }
    let (params, ret) = classfile::parse_method_desc(&hd.desc).map(|(p, r)| (p.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.to_string())).unwrap_or_default();
    let full_params: Vec<String> = match &hd.target {
        Target::Static(m) | Target::Virtual(m) | Target::Special(m) | Target::Constructor(m, _) => {
            let method = vm.method(*m).clone();
            let (p, _) = classfile::parse_method_desc(&method.desc).unwrap_or_default();
            let mut v: Vec<String> = p.iter().map(|s| s.to_string()).collect();
            if !method.is_static() && !matches!(hd.target, Target::Constructor(..)) {
                v.insert(0, "Ljava/lang/Object;".into());
            }
            v
        }
        _ => {
            // The handle's own type, before anything was bound.
            let mut v = params.clone();
            for (pos, _) in &hd.bound {
                v.insert((*pos).min(v.len()), "Ljava/lang/Object;".into());
            }
            v
        }
    };
    if all.len() != full_params.len() && !matches!(hd.target, Target::Identity | Target::Constant(_)) {
        let msg = format!("expected {} arguments, got {}", full_params.len(), all.len());
        return Err(vm.throw_new("java/lang/invoke/WrongMethodTypeException", Some(&msg)));
    }
    for (v, p) in all.iter_mut().zip(&full_params) {
        *v = vm.adapt(*v, p)?;
    }
    let field = |vm: &Vm, owner: ClassId, slot: usize, is_static: bool, obj: ObjId| -> Option<Value> {
        if is_static {
            vm.class(owner).statics.get(slot).copied()
        } else {
            vm.obj(obj).and_then(|o| o.fields.get(slot).copied())
        }
    };
    match hd.target {
        Target::Static(m) => {
            let c = vm.method(m).class;
            vm.ensure_init(c)?;
            vm.invoke(m, &all)
        }
        Target::Special(m) => {
            if all.first().is_none_or(|v| v.r() == 0) {
                return Err(vm.npe(None));
            }
            vm.invoke(m, &all)
        }
        Target::Virtual(m) => {
            let recv = all.first().map_or(0, |v| v.r());
            let Some(c) = vm.class_of(recv) else { return Err(vm.npe(None)) };
            let nd = vm.method(m).nd;
            let sel = vm.select(c, nd)?;
            vm.invoke(sel, &all)
        }
        Target::Constructor(m, class) => {
            vm.ensure_init(class)?;
            let r = vm.new_instance(class)?;
            let mut call = vec![Value::Ref(r)];
            call.extend(all);
            vm.invoke(m, &call)?;
            Ok(Value::Ref(r))
        }
        Target::Getter { owner, slot, is_static } => {
            if is_static {
                vm.ensure_init(owner)?;
            }
            let obj = if is_static { 0 } else { all.first().map_or(0, |v| v.r()) };
            if !is_static && obj == 0 {
                return Err(vm.npe(None));
            }
            Ok(field(vm, owner, slot, is_static, obj).unwrap_or_default())
        }
        Target::Setter { owner, slot, is_static } => {
            if is_static {
                vm.ensure_init(owner)?;
            }
            let (obj, v) = if is_static { (0, all.first().copied().unwrap_or_default()) } else { (all.first().map_or(0, |v| v.r()), all.get(1).copied().unwrap_or_default()) };
            set_field(vm, owner, slot, is_static, obj, v)?;
            let _ = ret;
            Ok(Value::Top)
        }
        Target::Constant(v) => Ok(v),
        Target::Identity => Ok(all.first().copied().unwrap_or_default()),
        Target::ArrayGetter => {
            let (arr, i) = (all[0].r(), all[1].i());
            array_get(vm, arr, i)
        }
        Target::ArraySetter => {
            let (arr, i) = (all[0].r(), all[1].i());
            super::lang::array_store(vm, arr, i.max(0) as usize, all[2])?;
            Ok(Value::Top)
        }
        Target::VarField { .. } | Target::VarArray => Err(Exc::Internal("a VarHandle called as a MethodHandle".into())),
    }
}

fn set_field(vm: &mut Vm, owner: ClassId, slot: usize, is_static: bool, obj: ObjId, v: Value) -> Res<()> {
    if is_static {
        if let Some(s) = vm.classes[owner as usize].statics.get_mut(slot) {
            *s = v;
        }
    } else {
        let Some(o) = vm.obj_mut(obj) else { return Err(vm.npe(None)) };
        if let Some(f) = o.fields.get_mut(slot) {
            *f = v;
        }
    }
    Ok(())
}

fn array_get(vm: &mut Vm, arr: ObjId, i: i32) -> Res<Value> {
    let Some(a) = vm.array(arr) else { return Err(vm.npe(None)) };
    match a.get(i.max(0) as usize).filter(|_| i >= 0) {
        Some(v) => Ok(v),
        None => {
            let msg = format!("Index {i} out of bounds for length {}", a.len());
            Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)))
        }
    }
}

/// `MethodHandle.invoke` and `invokeExact` with the descriptor of the call
/// site that runs this native.
fn invoke_polymorphic(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let site = vm.method(vm.native_method).desc.clone();
    let h = this(vm, a)?;
    let Some(hd) = handle(vm, h).cloned() else { return Err(vm.npe(None)) };
    if matches!(hd.target, Target::VarField { .. } | Target::VarArray) {
        let name = vm.method(vm.native_method).name.clone();
        return var_access(vm, h, &name, &site, &a[1..]);
    }
    let result = call_handle(vm, h, &a[1..])?;
    let (_, wanted) = classfile::parse_method_desc(&site).unwrap_or_default();
    let (_, produced) = classfile::parse_method_desc(&hd.desc).unwrap_or_default();
    let (wanted, produced) = (wanted.to_string(), produced.to_string());
    vm.adapt_return(result, &produced, &wanted)
}

pub fn is_polymorphic(vm: &Vm, c: ClassId, name: &str) -> bool {
    match vm.class_name(c) {
        "java/lang/invoke/MethodHandle" => matches!(name, "invoke" | "invokeExact"),
        "java/lang/invoke/VarHandle" => VAR_MODES.contains(&name),
        _ => false,
    }
}

const VAR_MODES: &[&str] = &[
    "get", "set", "getVolatile", "setVolatile", "getOpaque", "setOpaque", "getAcquire", "setRelease", "compareAndSet", "compareAndExchange",
    "compareAndExchangeAcquire", "compareAndExchangeRelease", "weakCompareAndSet", "weakCompareAndSetPlain", "weakCompareAndSetAcquire",
    "weakCompareAndSetRelease", "getAndSet", "getAndSetAcquire", "getAndSetRelease", "getAndAdd", "getAndAddAcquire", "getAndAddRelease",
    "getAndBitwiseOr", "getAndBitwiseAnd", "getAndBitwiseXor",
];

/// The native method a signature-polymorphic call with `desc` runs: one per
/// descriptor, made on first use.
pub fn polymorphic_target(vm: &mut Vm, c: ClassId, name: &str, desc: &str) -> Res<MethodId> {
    let key = (c, name.to_string(), desc.to_string());
    if let Some(&m) = vm.jdk.polymorphic.get(&key) {
        return Ok(m);
    }
    let m = vm.add_method(c, name, desc, ACC_PUBLIC | ACC_FINAL | ACC_NATIVE | ACC_VARARGS, MethodBody::Native(invoke_polymorphic), None)?;
    vm.jdk.polymorphic.insert(key, m);
    Ok(m)
}

/// A `VarHandle` access: `get`, `set`, `compareAndSet`, `getAndAdd`...
fn var_access(vm: &mut Vm, h: ObjId, mode: &str, site: &str, args: &[Value]) -> Res<Value> {
    let Some(hd) = handle(vm, h).cloned() else { return Err(vm.npe(None)) };
    let (coords, rest): (usize, &[Value]) = match hd.target {
        Target::VarField { is_static: true, .. } => (0, args),
        Target::VarField { .. } => (1, &args[1.min(args.len())..]),
        _ => (2, &args[2.min(args.len())..]),
    };
    let read = |vm: &mut Vm| -> Res<Value> {
        match hd.target {
            Target::VarField { owner, slot, is_static } => {
                let obj = if is_static { 0 } else { args[0].r() };
                if !is_static && obj == 0 {
                    return Err(vm.npe(None));
                }
                Ok(if is_static { vm.class(owner).statics[slot] } else { vm.obj(obj).map(|o| o.fields[slot]).unwrap_or_default() })
            }
            _ => array_get(vm, args[0].r(), args[1].i()),
        }
    };
    let write = |vm: &mut Vm, v: Value| -> Res<()> {
        match hd.target {
            Target::VarField { owner, slot, is_static } => set_field(vm, owner, slot, is_static, if is_static { 0 } else { args[0].r() }, v),
            _ => super::lang::array_store(vm, args[0].r(), args[1].i().max(0) as usize, v),
        }
    };
    let value_desc = if hd.var_desc.is_empty() { "Ljava/lang/Object;".to_string() } else { hd.var_desc.clone() };
    let mut vals = Vec::new();
    for v in rest {
        vals.push(vm.adapt(*v, &value_desc)?);
    }
    let _ = coords;
    let same = |a: Value, b: Value| match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (x, y) => x == y,
    };
    let result = match mode {
        "get" | "getVolatile" | "getOpaque" | "getAcquire" => read(vm)?,
        "set" | "setVolatile" | "setOpaque" | "setRelease" => {
            write(vm, vals[0])?;
            Value::Top
        }
        m if m.starts_with("compareAndSet") || m.starts_with("weakCompareAndSet") => {
            let cur = read(vm)?;
            let hit = same(cur, vals[0]);
            if hit {
                write(vm, vals[1])?;
            }
            Value::Int(hit as i32)
        }
        m if m.starts_with("compareAndExchange") => {
            let cur = read(vm)?;
            if same(cur, vals[0]) {
                write(vm, vals[1])?;
            }
            cur
        }
        m if m.starts_with("getAndSet") => {
            let cur = read(vm)?;
            write(vm, vals[0])?;
            cur
        }
        m if m.starts_with("getAndAdd") || m.starts_with("getAndBitwise") => {
            let cur = read(vm)?;
            let d = vals[0];
            let new = match (cur, d) {
                (Value::Int(x), Value::Int(y)) => Value::Int(match m {
                    _ if m.starts_with("getAndAdd") => x.wrapping_add(y),
                    _ if m.contains("Or") => x | y,
                    _ if m.contains("And") => x & y,
                    _ => x ^ y,
                }),
                (Value::Long(x), Value::Long(y)) => Value::Long(match m {
                    _ if m.starts_with("getAndAdd") => x.wrapping_add(y),
                    _ if m.contains("Or") => x | y,
                    _ if m.contains("And") => x & y,
                    _ => x ^ y,
                }),
                (Value::Float(x), Value::Float(y)) => Value::Float(x + y),
                (Value::Double(x), Value::Double(y)) => Value::Double(x + y),
                _ => return Err(vm.throw_new("java/lang/UnsupportedOperationException", None)),
            };
            write(vm, new)?;
            cur
        }
        _ => return Err(Exc::Unsupported(format!("VarHandle.{mode}"))),
    };
    let (_, wanted) = classfile::parse_method_desc(site).unwrap_or_default();
    let produced = if result == Value::Top { "V".to_string() } else if matches!(result, Value::Int(_)) && (mode.contains("ompareAndSet")) { "Z".to_string() } else { value_desc };
    let wanted = wanted.to_string();
    vm.adapt_return(result, &produced, &wanted)
}

fn handle_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(hd) = handle(vm, t).cloned() else { return Err(vm.npe(None)) };
    match op {
        "type" => ret_ref(method_type_of(vm, &hd.desc)?),
        "bindTo" => {
            let (params, ret) = classfile::parse_method_desc(&hd.desc).map(|(p, r)| (p.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.to_string())).unwrap_or_default();
            if params.is_empty() || !params[0].starts_with(['L', '[']) {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("no leading reference parameter")));
            }
            let mut h2 = hd.clone();
            h2.bound.insert(0, (0, arg(a, 1)));
            h2.desc = format!("({}){ret}", params[1..].concat());
            ret_ref(new_handle(vm, h2)?)
        }
        "asType" => {
            let mut h2 = hd.clone();
            h2.desc = method_type_desc(vm, arg(a, 1).r()).unwrap_or(hd.desc);
            ret_ref(new_handle(vm, h2)?)
        }
        "invokeWithArguments" => {
            let r = arg(a, 1).r();
            let items: Vec<Value> = if r == 0 {
                Vec::new()
            } else if vm.array(r).is_some() {
                vm.array_values(r)
            } else {
                super::util::iterate(vm, r)?
            };
            let result = call_handle(vm, t, &items)?;
            let (_, produced) = classfile::parse_method_desc(&hd.desc).unwrap_or_default();
            let produced = produced.to_string();
            vm.adapt_return(result, &produced, "Ljava/lang/Object;")
        }
        "toString" => {
            let text = type_text(vm, &hd.desc)?;
            ret_str(vm, &format!("MethodHandle{text}"))
        }
        "varType" => {
            let d = hd.var_desc.clone();
            ret_ref(mirror_of_desc(vm, &d)?)
        }
        "withInvokeExactBehavior" | "withInvokeBehavior" | "asFixedArity" => ret_ref(t),
        "isVarargsCollector" => ret_bool(false),
        _ => Err(Exc::Unsupported(format!("MethodHandle.{op}"))),
    }
}

fn handles_static(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "lookup" => {
            let c = caller_class(vm);
            ret_ref(new_lookup(vm, c)?)
        }
        "publicLookup" => {
            let c = vm.known.object;
            ret_ref(new_lookup(vm, c)?)
        }
        "privateLookupIn" => {
            let c = vm.mirror_class(arg(a, 0).r()).unwrap_or(vm.known.object);
            ret_ref(new_lookup(vm, c)?)
        }
        "constant" => {
            let d = class_desc(vm, arg(a, 0).r())?;
            let v = vm.adapt(arg(a, 1), &d)?;
            ret_ref(new_handle(vm, Handle { target: Target::Constant(v), bound: Vec::new(), desc: format!("(){d}"), var_desc: String::new() })?)
        }
        "identity" => {
            let d = class_desc(vm, arg(a, 0).r())?;
            ret_ref(new_handle(vm, Handle { target: Target::Identity, bound: Vec::new(), desc: format!("({d}){d}"), var_desc: String::new() })?)
        }
        "insertArguments" => {
            let h = arg(a, 0).r();
            let Some(mut hd) = handle(vm, h).cloned() else { return Err(vm.npe(None)) };
            let pos = arg(a, 1).i().max(0) as usize;
            let values = vm.array_values(arg(a, 2).r());
            let (mut params, ret) = classfile::parse_method_desc(&hd.desc).map(|(p, r)| (p.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.to_string())).unwrap_or_default();
            if pos + values.len() > params.len() {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("too many values to insert")));
            }
            // Positions are in terms of the handle before binding: shift past earlier bound ones.
            let before = hd.bound.iter().filter(|(p, _)| *p <= pos).count();
            for (i, v) in values.iter().enumerate() {
                hd.bound.push((pos + before + i, *v));
            }
            hd.bound.sort_by_key(|(p, _)| *p);
            params.drain(pos..pos + values.len());
            hd.desc = format!("({}){ret}", params.concat());
            ret_ref(new_handle(vm, hd)?)
        }
        "arrayElementGetter" | "arrayElementSetter" | "arrayElementVarHandle" => {
            let d = class_desc(vm, arg(a, 0).r())?;
            let elem = d[1..].to_string();
            let (target, desc) = match op {
                "arrayElementGetter" => (Target::ArrayGetter, format!("({d}I){elem}")),
                "arrayElementSetter" => (Target::ArraySetter, format!("({d}I{elem})V")),
                _ => (Target::VarArray, format!("({d}I){elem}")),
            };
            ret_ref(new_handle(vm, Handle { target, bound: Vec::new(), desc, var_desc: elem })?)
        }
        _ => Err(Exc::Unsupported(format!("MethodHandles.{op}"))),
    }
}

/// A `CONSTANT_MethodHandle` loaded by `ldc`.
pub fn handle_constant(vm: &mut Vm, cf: &ClassFile, index: u16) -> Res<ObjId> {
    let h = handle_ref(cf, index)?;
    let c = vm.class_of_desc_or_name(&h.class)?;
    let nd = vm.nd(&h.name, &h.desc);
    let handle = match h.kind {
        1..=4 => {
            let Some((owner, f, is_static)) = vm.resolve_field(c, nd) else {
                let msg = format!("{}.{}", h.class.replace('/', "."), h.name);
                return Err(vm.throw_new("java/lang/NoSuchFieldError", Some(&msg)));
            };
            let recv = if is_static { String::new() } else { vm.descriptor_of(c) };
            match h.kind {
                1 | 2 => Handle { target: Target::Getter { owner, slot: f.slot, is_static }, bound: Vec::new(), desc: format!("({recv}){}", h.desc), var_desc: String::new() },
                _ => Handle { target: Target::Setter { owner, slot: f.slot, is_static }, bound: Vec::new(), desc: format!("({recv}{})V", h.desc), var_desc: String::new() },
            }
        }
        _ => {
            let Some(m) = vm.resolve_method(c, nd) else {
                let msg = format!("'{}'", vm.pretty_method(&h.name, &h.desc));
                return Err(vm.throw_new("java/lang/NoSuchMethodError", Some(&msg)));
            };
            match h.kind {
                6 => Handle { target: Target::Static(m), bound: Vec::new(), desc: h.desc.to_string(), var_desc: String::new() },
                8 => {
                    let (params, _) = classfile::parse_method_desc(&h.desc).unwrap_or_default();
                    Handle { target: Target::Constructor(m, c), bound: Vec::new(), desc: format!("({}){}", params.concat(), vm.descriptor_of(c)), var_desc: String::new() }
                }
                7 => Handle { target: Target::Special(m), bound: Vec::new(), desc: receiver_desc(vm, c, &h.desc), var_desc: String::new() },
                _ => Handle { target: Target::Virtual(m), bound: Vec::new(), desc: receiver_desc(vm, c, &h.desc), var_desc: String::new() },
            }
        }
    };
    let r = new_handle(vm, handle)?;
    vm.pinned.push(r);
    Ok(r)
}

/// `ConstantBootstraps.invoke`: the static factories javac names in dynamic
/// constants (`ClassDesc.of`, `EnumDesc.of`), and any other static method.
pub fn constant_invoke(vm: &mut Vm, h: &HandleRef, args: &[Value]) -> Res<Value> {
    match (&*h.class, &*h.name) {
        ("java/lang/constant/ClassDesc", "of") => {
            let c = vm.load("java/lang/constant/ClassDesc$Native")?;
            let r = vm.new_instance(c)?;
            if let Some(o) = vm.obj_mut(r) {
                o.data = Data::Jdk(Box::new(State::Generic(vec![args[0]])));
            }
            Ok(Value::Ref(r))
        }
        ("java/lang/Enum$EnumDesc", "of") => {
            let class_name = match state(vm, args[0].r()) {
                Some(State::Generic(v)) => vm.string(v[0].r()).unwrap_or_default(),
                _ => String::new(),
            };
            let enum_class = vm.load(&class_name.replace('.', "/"))?;
            let name = vm.string_nn(args[1].r())?;
            let c = vm.load("java/lang/Enum$EnumDesc")?;
            let r = vm.new_instance(c)?;
            if let Some(o) = vm.obj_mut(r) {
                o.data = Data::Jdk(Box::new(State::Generic(vec![args[0], args[1]])));
            }
            vm.jdk.enum_descs.insert(r, (enum_class, name.into()));
            Ok(Value::Ref(r))
        }
        (class, name) => {
            let c = vm.load(class)?;
            let Some(m) = vm.find_method(c, name, &h.desc) else {
                return Err(Exc::Unsupported(format!("constant bootstrap target {class}.{name}")));
            };
            vm.ensure_init(c)?;
            vm.invoke(m, args)
        }
    }
}

/// A `Function` that calls a getter on its argument (`Map.Entry.comparingByKey`).
pub fn getter_function(vm: &mut Vm, getter: &str) -> Res<ObjId> {
    let c = vm.load("java/util/function/Function$Getter")?;
    let r = vm.new_instance(c)?;
    let n = vm.intern(getter)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(n)])));
    }
    Ok(r)
}

fn getter_apply(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let name = match state(vm, t) {
        Some(State::Generic(v)) => vm.string(v[0].r()).unwrap_or_default(),
        _ => String::new(),
    };
    let target = arg(a, 1).r();
    if target == 0 {
        return Err(vm.npe(None));
    }
    vm.invoke_virtual(target, &name, "()Ljava/lang/Object;", &[])
}

static LOOKUP_METHODS: &[NativeMethod] = natives![
    "findStatic" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findStatic")),
    "findVirtual" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findVirtual")),
    "findSpecial" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findSpecial")),
    "findConstructor" "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findConstructor")),
    "findGetter" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findGetter")),
    "findSetter" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findSetter")),
    "findStaticGetter" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findStaticGetter")),
    "findStaticSetter" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "findStaticSetter")),
    "findVarHandle" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;" M => Some(|vm, a| lookup_op(vm, a, "findVarHandle")),
    "findStaticVarHandle" "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;" M => Some(|vm, a| lookup_op(vm, a, "findStaticVarHandle")),
    "unreflect" "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "unreflect")),
    "unreflectSpecial" "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "unreflectSpecial")),
    "unreflectConstructor" "(Ljava/lang/reflect/Constructor;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "unreflectConstructor")),
    "unreflectGetter" "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "unreflectGetter")),
    "unreflectSetter" "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| lookup_op(vm, a, "unreflectSetter")),
    "unreflectVarHandle" "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/VarHandle;" M => Some(|vm, a| lookup_op(vm, a, "unreflectVarHandle")),
    "lookupClass" "()Ljava/lang/Class;" M => Some(|vm, a| lookup_op(vm, a, "lookupClass")),
    "lookupModes" "()I" M => Some(|vm, a| lookup_op(vm, a, "lookupModes")),
    "in" "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;" M => Some(|vm, a| lookup_op(vm, a, "in")),
    "findClass" "(Ljava/lang/String;)Ljava/lang/Class;" M => Some(|vm, a| lookup_op(vm, a, "findClass")),
    "accessClass" "(Ljava/lang/Class;)Ljava/lang/Class;" M => Some(|vm, a| lookup_op(vm, a, "accessClass")),
    "ensureInitialized" "(Ljava/lang/Class;)Ljava/lang/Class;" M => Some(|vm, a| lookup_op(vm, a, "ensureInitialized")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| lookup_op(vm, a, "toString")),
];

static CLASSES: &[NativeClass] = &[
    NativeClass { name: "java/lang/invoke/MethodHandle", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | 0x0400, methods: natives![
        "type" "()Ljava/lang/invoke/MethodType;" M => Some(|vm, a| handle_op(vm, a, "type")),
        "bindTo" "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| handle_op(vm, a, "bindTo")),
        "asType" "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| handle_op(vm, a, "asType")),
        "invokeWithArguments" "([Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| handle_op(vm, a, "invokeWithArguments")),
        "invokeWithArguments" "(Ljava/util/List;)Ljava/lang/Object;" M => Some(|vm, a| handle_op(vm, a, "invokeWithArguments")),
        "asFixedArity" "()Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| handle_op(vm, a, "asFixedArity")),
        "isVarargsCollector" "()Z" M => Some(|vm, a| handle_op(vm, a, "isVarargsCollector")),
        "withVarargs" "(Z)Ljava/lang/invoke/MethodHandle;" M => Some(|vm, a| handle_op(vm, a, "asFixedArity")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| handle_op(vm, a, "toString")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/lang/invoke/VarHandle", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | 0x0400, methods: natives![
        "varType" "()Ljava/lang/Class;" M => Some(|vm, a| handle_op(vm, a, "varType")),
        "withInvokeExactBehavior" "()Ljava/lang/invoke/VarHandle;" M => Some(|vm, a| handle_op(vm, a, "withInvokeExactBehavior")),
        "withInvokeBehavior" "()Ljava/lang/invoke/VarHandle;" M => Some(|vm, a| handle_op(vm, a, "withInvokeBehavior")),
        "fullFence" "()V" S => Some(|_, _| void()),
        "acquireFence" "()V" S => Some(|_, _| void()),
        "releaseFence" "()V" S => Some(|_, _| void()),
        "loadLoadFence" "()V" S => Some(|_, _| void()),
        "storeStoreFence" "()V" S => Some(|_, _| void()),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/lang/invoke/MethodType", super_name: Some("java/lang/Object"), interfaces: &["java/io/Serializable"], access: PUBLIC | FINAL, methods: natives![
        "methodType" "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "methodType")),
        "methodType" "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "methodType")),
        "methodType" "(Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "methodType")),
        "methodType" "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "methodType")),
        "methodType" "(Ljava/lang/Class;Ljava/util/List;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "methodType")),
        "methodType" "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "methodType")),
        "genericMethodType" "(I)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "generic")),
        "fromMethodDescriptorString" "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;" S => Some(|vm, a| method_type_op(vm, a, "fromDescriptor")),
        "parameterCount" "()I" M => Some(|vm, a| method_type_op(vm, a, "parameterCount")),
        "parameterType" "(I)Ljava/lang/Class;" M => Some(|vm, a| method_type_op(vm, a, "parameterType")),
        "returnType" "()Ljava/lang/Class;" M => Some(|vm, a| method_type_op(vm, a, "returnType")),
        "parameterArray" "()[Ljava/lang/Class;" M => Some(|vm, a| method_type_op(vm, a, "parameterArray")),
        "parameterList" "()Ljava/util/List;" M => Some(|vm, a| method_type_op(vm, a, "parameterList")),
        "changeReturnType" "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "changeReturnType")),
        "changeParameterType" "(ILjava/lang/Class;)Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "changeParameterType")),
        "appendParameterTypes" "([Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "appendParameterTypes")),
        "insertParameterTypes" "(I[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "insertParameterTypes")),
        "dropParameterTypes" "(II)Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "dropParameterTypes")),
        "toMethodDescriptorString" "()Ljava/lang/String;" M => Some(|vm, a| method_type_op(vm, a, "toMethodDescriptorString")),
        "descriptorString" "()Ljava/lang/String;" M => Some(|vm, a| method_type_op(vm, a, "descriptorString")),
        "wrap" "()Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "wrap")),
        "unwrap" "()Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "unwrap")),
        "erase" "()Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "erase")),
        "generic" "()Ljava/lang/invoke/MethodType;" M => Some(|vm, a| method_type_op(vm, a, "generic")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| method_type_op(vm, a, "toString")),
        "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| method_type_op(vm, a, "equals")),
        "hashCode" "()I" M => Some(|vm, a| method_type_op(vm, a, "hashCode")),
    ], statics: &[], clinit: None, data: None },
    class("java/lang/invoke/MethodHandles", Some("java/lang/Object"), &[], PUBLIC, natives![
        "lookup" "()Ljava/lang/invoke/MethodHandles$Lookup;" S => Some(|vm, a| handles_static(vm, a, "lookup")),
        "publicLookup" "()Ljava/lang/invoke/MethodHandles$Lookup;" S => Some(|vm, a| handles_static(vm, a, "publicLookup")),
        "privateLookupIn" "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;" S => Some(|vm, a| handles_static(vm, a, "privateLookupIn")),
        "constant" "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;" S => Some(|vm, a| handles_static(vm, a, "constant")),
        "identity" "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" S => Some(|vm, a| handles_static(vm, a, "identity")),
        "insertArguments" "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;" S => Some(|vm, a| handles_static(vm, a, "insertArguments")),
        "arrayElementGetter" "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" S => Some(|vm, a| handles_static(vm, a, "arrayElementGetter")),
        "arrayElementSetter" "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;" S => Some(|vm, a| handles_static(vm, a, "arrayElementSetter")),
        "arrayElementVarHandle" "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;" S => Some(|vm, a| handles_static(vm, a, "arrayElementVarHandle")),
    ]),
    NativeClass { name: "java/lang/invoke/MethodHandles$Lookup", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: LOOKUP_METHODS, statics: &[], clinit: None, data: None },
    class("java/lang/invoke/CallSite", Some("java/lang/Object"), &[], PUBLIC | 0x0400, &[]),
    class("java/lang/constant/ClassDesc", None, &[], super::INTERFACE, natives!["descriptorString" "()Ljava/lang/String;" A => None]),
    NativeClass { name: "java/lang/constant/ClassDesc$Native", super_name: Some("java/lang/Object"), interfaces: &["java/lang/constant/ClassDesc"], access: FINAL, methods: natives![], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/lang/Enum$EnumDesc", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: natives![], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/function/Function$Getter", super_name: Some("java/lang/Object"), interfaces: &["java/util/function/Function"], access: FINAL, methods: natives!["apply" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(getter_apply)], statics: &[], clinit: None, data: None },
];

#[allow(dead_code)]
fn unused(_: ClassKind, _: Array) {
    let _ = ACC_STATIC;
}

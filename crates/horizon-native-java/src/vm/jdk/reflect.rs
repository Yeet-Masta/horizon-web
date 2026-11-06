//! Reflection: `Class`, `Method`, `Constructor`, `Field`, `Array`, `Modifier`,
//! proxies, annotations and the class loader a program sees.
use super::{A, FINAL, INTERFACE, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, state_mut, this, void};
use crate::natives;
use crate::vm::class::{ACC_ABSTRACT, ACC_ANNOTATION, ACC_ENUM, ACC_FINAL, ACC_INTERFACE, ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC, ACC_SYNTHETIC, ACC_VARARGS, ClassKind, InitState, MethodBody, desc_to_java};
use crate::vm::classfile::{self, ClassFile, Const, Reader};
use crate::vm::interp::Vm;
use crate::vm::{Array, ClassId, Data, Exc, MethodId, NameDesc, ObjId, Res, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

/// What a reflection object stands for.
#[derive(Debug, Clone)]
pub enum Member {
    Method(MethodId),
    Constructor(MethodId),
    Field { owner: ClassId, name: Arc<str>, desc: Arc<str>, slot: usize, is_static: bool, access: u16, annotations: Option<Arc<[u8]>> },
    RecordComponent { record: ClassId, index: usize },
    Parameter { method: MethodId, index: usize },
}

#[derive(Debug, Clone)]
pub struct Reflected {
    pub member: Member,
    pub accessible: bool,
}

/// A proxy class (or an annotation's implementation) the VM made.
#[derive(Debug, Clone, Default)]
pub struct ProxyClass {
    pub class: ClassId,
    pub interfaces: Vec<ClassId>,
    /// The `Method` object each of its methods hands the handler.
    pub methods: HashMap<NameDesc, ObjId>,
    /// An annotation type's implementation: the annotation interface.
    pub annotation: Option<ClassId>,
}

pub fn member(vm: &Vm, r: ObjId) -> Option<Member> {
    match state(vm, r) {
        Some(State::Reflect(m)) => Some(m.member.clone()),
        _ => None,
    }
}

fn reflected(vm: &Vm, r: ObjId) -> Option<Reflected> {
    match state(vm, r) {
        Some(State::Reflect(m)) => Some((**m).clone()),
        _ => None,
    }
}

fn new_member(vm: &mut Vm, class: &str, m: Member) -> Res<ObjId> {
    let c = vm.load(class)?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Reflect(Box::new(Reflected { member: m, accessible: false }))));
    }
    Ok(r)
}

fn method_object(vm: &mut Vm, m: MethodId) -> Res<ObjId> {
    if &*vm.method(m).name == "<init>" {
        new_member(vm, "java/lang/reflect/Constructor", Member::Constructor(m))
    } else {
        new_member(vm, "java/lang/reflect/Method", Member::Method(m))
    }
}

fn field_object(vm: &mut Vm, owner: ClassId, f: &crate::vm::class::FieldInfo, is_static: bool) -> Res<ObjId> {
    new_member(
        vm,
        "java/lang/reflect/Field",
        Member::Field { owner, name: f.name.clone(), desc: f.desc.clone(), slot: f.slot, is_static, access: f.access, annotations: f.annotations.clone() },
    )
}

fn mirror_class(vm: &mut Vm, r: ObjId) -> Res<ClassId> {
    match vm.mirror_class(r) {
        Some(c) => Ok(c),
        None => Err(vm.npe(None)),
    }
}

fn this_class(vm: &mut Vm, a: &[Value]) -> Res<ClassId> {
    let t = this(vm, a)?;
    mirror_class(vm, t)
}

fn mirror_array(vm: &mut Vm, classes: &[ClassId]) -> Res<Value> {
    let mut mirrors = Vec::with_capacity(classes.len());
    for &c in classes {
        mirrors.push(vm.mirror(c)?);
    }
    ret_ref(vm.new_ref_array("Ljava/lang/Class;", &mirrors)?)
}

fn param_classes(vm: &mut Vm, desc: &str) -> Res<Vec<ClassId>> {
    let (params, _) = classfile::parse_method_desc(desc).unwrap_or_default();
    let params: Vec<String> = params.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    for p in params {
        out.push(vm.class_of_desc(&p)?);
    }
    Ok(out)
}

/// Descriptors of the classes in a `Class[]` (null: no parameters).
fn descs_of(vm: &mut Vm, arr: ObjId) -> Res<String> {
    if arr == 0 {
        return Ok(String::new());
    }
    let mut out = String::new();
    for m in vm.ref_elements(arr) {
        let c = mirror_class(vm, m)?;
        out.push_str(&vm.descriptor_of(c));
    }
    Ok(out)
}

// ------------------------------------------------------------------ Class

/// `Class.getModifiers`: the inner-class flags of a member class, the class
/// flags otherwise, without `ACC_SUPER`.
pub fn class_modifiers(vm: &Vm, c: ClassId) -> i32 {
    let class = vm.class(c);
    match class.kind {
        ClassKind::Primitive(_) => return (ACC_PUBLIC | ACC_FINAL | ACC_ABSTRACT) as i32,
        ClassKind::Array { component, .. } => {
            let inner = class_modifiers(vm, component);
            return (inner & (ACC_PUBLIC | ACC_PRIVATE | ACC_PROTECTED) as i32) | (ACC_FINAL | ACC_ABSTRACT) as i32;
        }
        _ => {}
    }
    if let Some(cf) = class.class_file() {
        for ic in &cf.inner_classes {
            if cf.class_name(ic.inner).ok().as_deref() == Some(&*class.name) {
                return (ic.access & !0x20) as i32;
            }
        }
    }
    (class.access & !0x20) as i32
}

fn is_enum_class(vm: &Vm, c: ClassId) -> bool {
    let class = vm.class(c);
    class.access & ACC_ENUM != 0 && class.super_class.is_some_and(|s| vm.class_name(s) == "java/lang/Enum")
}

fn is_record(vm: &Vm, c: ClassId) -> bool {
    vm.class(c).class_file().is_some_and(|cf| cf.record.is_some()) && vm.class(c).super_class.is_some_and(|s| vm.class_name(s) == "java/lang/Record")
}

/// The class's own InnerClasses entry: (outer, simple name present, access).
fn inner_entry(vm: &Vm, c: ClassId) -> Option<(Option<String>, bool)> {
    let class = vm.class(c);
    let cf = class.class_file()?;
    for ic in &cf.inner_classes {
        if cf.class_name(ic.inner).ok().as_deref() == Some(&*class.name) {
            let outer = if ic.outer == 0 { None } else { cf.class_name(ic.outer).ok().map(|s| s.to_string()) };
            return Some((outer, ic.name != 0));
        }
    }
    None
}

fn canonical_name(vm: &Vm, c: ClassId) -> Option<String> {
    let class = vm.class(c);
    match class.kind {
        ClassKind::Array { component, .. } => return canonical_name(vm, component).map(|n| n + "[]"),
        ClassKind::Primitive(_) => return Some(class.java_name()),
        ClassKind::Synthetic => return None,
        _ => {}
    }
    match inner_entry(vm, c) {
        Some((Some(outer), true)) => {
            let outer_id = vm.find_loaded(&outer)?;
            Some(format!("{}.{}", canonical_name(vm, outer_id)?, super::lang::simple_name(vm, c)))
        }
        Some(_) => None,
        None => Some(class.java_name()),
    }
}

fn type_name(vm: &Vm, c: ClassId) -> String {
    match vm.class(c).kind {
        ClassKind::Array { component, .. } => type_name(vm, component) + "[]",
        _ => vm.class(c).java_name(),
    }
}

fn class_to_string(vm: &Vm, c: ClassId) -> String {
    let class = vm.class(c);
    if matches!(class.kind, ClassKind::Primitive(_)) {
        return class.java_name();
    }
    format!("{} {}", if class.is_interface() { "interface" } else { "class" }, class.java_name())
}

/// Methods a class declares, in declaration order, as reflection lists them
/// (no `<clinit>`; constructors apart).
fn declared_methods(vm: &Vm, c: ClassId, constructors: bool) -> Vec<MethodId> {
    vm.class(c)
        .method_order
        .iter()
        .copied()
        .filter(|&m| {
            let method = vm.method(m);
            let is_ctor = &*method.name == "<init>";
            &*method.name != "<clinit>" && is_ctor == constructors && !matches!(method.body, MethodBody::Native(_) if method.access & 0x100 != 0 && vm.class_name(c) == "java/lang/invoke/MethodHandle")
        })
        .collect()
}

/// `getMethods`: public methods of the class, its superclasses and its
/// interfaces, the most specific of each signature.
fn public_methods(vm: &Vm, c: ClassId) -> Vec<MethodId> {
    let mut out: Vec<MethodId> = Vec::new();
    let mut seen: Vec<NameDesc> = Vec::new();
    let mut queue = vec![c];
    let mut visited = Vec::new();
    while let Some(k) = queue.first().copied() {
        queue.remove(0);
        if visited.contains(&k) {
            continue;
        }
        visited.push(k);
        for m in declared_methods(vm, k, false) {
            let method = vm.method(m);
            if method.access & ACC_PUBLIC != 0 && !seen.contains(&method.nd) && !(vm.class(k).is_interface() && method.is_static() && k != c) {
                seen.push(method.nd);
                out.push(m);
            }
        }
        let class = vm.class(k);
        if let Some(s) = class.super_class {
            queue.push(s);
        }
        queue.extend(class.interfaces.iter().copied());
    }
    out
}

fn all_fields(vm: &Vm, c: ClassId) -> Vec<(ClassId, crate::vm::class::FieldInfo, bool)> {
    let class = vm.class(c);
    let mut out: Vec<_> = Vec::new();
    // Declaration order: the class file's, statics and instance fields mixed.
    if let Some(cf) = class.class_file() {
        for f in &cf.fields {
            let found = class.fields.iter().find(|x| x.name == f.name).map(|x| (x.clone(), false)).or_else(|| class.static_fields.iter().find(|x| x.name == f.name).map(|x| (x.clone(), true)));
            if let Some((info, is_static)) = found {
                out.push((c, info, is_static));
            }
        }
    } else {
        out.extend(class.fields.iter().map(|f| (c, f.clone(), false)));
        out.extend(class.static_fields.iter().map(|f| (c, f.clone(), true)));
    }
    out
}

fn public_fields(vm: &Vm, c: ClassId) -> Vec<(ClassId, crate::vm::class::FieldInfo, bool)> {
    let mut out = Vec::new();
    let mut queue = vec![c];
    let mut visited = Vec::new();
    while let Some(k) = queue.pop() {
        if visited.contains(&k) {
            continue;
        }
        visited.push(k);
        out.extend(all_fields(vm, k).into_iter().filter(|(_, f, _)| f.access & ACC_PUBLIC != 0));
        let class = vm.class(k);
        let mut next: Vec<ClassId> = class.interfaces.clone();
        if let Some(s) = class.super_class {
            next.push(s);
        }
        next.reverse();
        queue.extend(next);
    }
    out
}

fn for_name(vm: &mut Vm, name_obj: ObjId, initialize: bool) -> Res<Value> {
    if name_obj == 0 {
        return Err(vm.npe(None));
    }
    let name = vm.string(name_obj).unwrap_or_default();
    let internal = name.replace('.', "/");
    let found = if internal.starts_with('[') {
        if internal.ends_with(';') || internal.trim_start_matches('[').len() == 1 { Some(vm.array_class(&internal)?) } else { None }
    } else if name.contains('/') {
        None
    } else {
        vm.try_load(&internal)?
    };
    let Some(c) = found else {
        return Err(vm.throw_new("java/lang/ClassNotFoundException", Some(&name)));
    };
    if initialize {
        vm.ensure_init(c)?;
    }
    ret_ref(vm.mirror(c)?)
}

fn class_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "forName" => return for_name(vm, arg(a, 0).r(), true),
        "forName3" => return for_name(vm, arg(a, 0).r(), arg(a, 1).z()),
        "forPrimitiveName" => {
            let n = vm.string_nn(arg(a, 0).r())?;
            let p = match n.as_str() {
                "boolean" => b'Z',
                "byte" => b'B',
                "char" => b'C',
                "short" => b'S',
                "int" => b'I',
                "long" => b'J',
                "float" => b'F',
                "double" => b'D',
                "void" => b'V',
                _ => return ret_ref(0),
            };
            let c = vm.primitive_class(p);
            return ret_ref(vm.mirror(c)?);
        }
        _ => {}
    }
    let c = this_class(vm, a)?;
    let t = arg(a, 0).r();
    match op {
        "getName" => {
            let n = vm.class(c).java_name();
            ret_ref(vm.intern(&n)?)
        }
        "getSimpleName" => {
            let n = super::lang::simple_name(vm, c);
            ret_str(vm, &n)
        }
        "getTypeName" => {
            let n = type_name(vm, c);
            ret_str(vm, &n)
        }
        "getCanonicalName" => match canonical_name(vm, c) {
            Some(n) => ret_str(vm, &n),
            None => ret_ref(0),
        },
        "getPackageName" => {
            let mut k = c;
            while let ClassKind::Array { component, .. } = vm.class(k).kind {
                k = component;
            }
            let n = match vm.class(k).kind {
                ClassKind::Primitive(_) => "java.lang".to_string(),
                _ => vm.class(k).name.rsplit_once('/').map_or(String::new(), |(p, _)| p.replace('/', ".")),
            };
            ret_str(vm, &n)
        }
        "getPackage" => {
            let pkg = class_op(vm, a, "getPackageName")?;
            let cls = vm.load("java/lang/Package")?;
            let r = vm.new_instance(cls)?;
            if let Some(o) = vm.obj_mut(r) {
                o.data = Data::Jdk(Box::new(State::Generic(vec![pkg])));
            }
            ret_ref(r)
        }
        "toString" => {
            let s = class_to_string(vm, c);
            ret_str(vm, &s)
        }
        "descriptorString" => {
            let d = vm.descriptor_of(c);
            ret_str(vm, &d)
        }
        "getSuperclass" => {
            let class = vm.class(c);
            match class.super_class {
                Some(s) if !class.is_interface() && !matches!(class.kind, ClassKind::Primitive(_)) => ret_ref(vm.mirror(s)?),
                _ => ret_ref(0),
            }
        }
        "getInterfaces" => {
            let i = vm.class(c).interfaces.clone();
            mirror_array(vm, &i)
        }
        "isInterface" => ret_bool(vm.class(c).is_interface()),
        "isArray" => ret_bool(vm.class(c).is_array()),
        "isPrimitive" => ret_bool(matches!(vm.class(c).kind, ClassKind::Primitive(_))),
        "isEnum" => ret_bool(is_enum_class(vm, c)),
        "isRecord" => ret_bool(is_record(vm, c)),
        "isAnnotation" => ret_bool(vm.class(c).access & ACC_ANNOTATION != 0),
        "isSynthetic" => ret_bool(vm.class(c).access & ACC_SYNTHETIC != 0),
        "isHidden" => ret_bool(matches!(vm.class(c).kind, ClassKind::Synthetic) && vm.class(c).name.contains("$$Lambda")),
        "isAnonymousClass" => ret_bool(matches!(inner_entry(vm, c), Some((_, false)))),
        "isLocalClass" => ret_bool(matches!(inner_entry(vm, c), Some((None, true)))),
        "isMemberClass" => ret_bool(matches!(inner_entry(vm, c), Some((Some(_), true)))),
        "isSealed" => ret_bool(vm.class(c).class_file().is_some_and(|cf| !cf.permitted.is_empty())),
        "getPermittedSubclasses" => {
            let Some(cf) = vm.class(c).class_file().cloned() else { return ret_ref(0) };
            if cf.permitted.is_empty() {
                return ret_ref(0);
            }
            let mut out = Vec::new();
            for &i in &cf.permitted {
                let n = cf.class_name(i).map_err(|e| Exc::Internal(e.0))?;
                out.push(vm.load(&n)?);
            }
            mirror_array(vm, &out)
        }
        "isInstance" => ret_bool(arg(a, 1).r() != 0 && vm.instance_of(arg(a, 1).r(), c)),
        "isAssignableFrom" => {
            let other = mirror_class(vm, arg(a, 1).r())?;
            let same = other == c;
            let primitive = matches!(vm.class(c).kind, ClassKind::Primitive(_)) || matches!(vm.class(other).kind, ClassKind::Primitive(_));
            ret_bool(same || (!primitive && vm.is_subclass(other, c)))
        }
        "cast" => {
            let o = arg(a, 1).r();
            if o != 0 && !vm.instance_of(o, c) {
                let from = vm.class_of(o).map(|k| vm.class(k).java_name()).unwrap_or_default();
                let msg = format!("Cannot cast {from} to {}", type_name(vm, c));
                return Err(vm.throw_new("java/lang/ClassCastException", Some(&msg)));
            }
            Ok(arg(a, 1))
        }
        "getComponentType" => match vm.class(c).kind {
            ClassKind::Array { component, .. } => ret_ref(vm.mirror(component)?),
            _ => ret_ref(0),
        },
        "arrayType" => {
            let k = vm.array_class_of(c)?;
            ret_ref(vm.mirror(k)?)
        }
        "getModifiers" => Ok(Value::Int(class_modifiers(vm, c))),
        "getClassLoader" => {
            if vm.class(c).class_file().is_none() {
                return ret_ref(0);
            }
            ret_ref(app_loader(vm)?)
        }
        "desiredAssertionStatus" => ret_bool(vm.options.assertions && vm.class(c).class_file().is_some()),
        "getEnumConstants" => {
            if !is_enum_class(vm, c) {
                return ret_ref(0);
            }
            let items = super::lang::enum_constants(vm, c)?;
            let d = vm.descriptor_of(c);
            ret_ref(vm.new_ref_array(&d, &items)?)
        }
        "newInstance" => {
            let Some(ctor) = vm.find_method(c, "<init>", "()V").filter(|&m| vm.method(m).class == c) else {
                let n = vm.class(c).java_name();
                return Err(vm.throw_new("java/lang/InstantiationException", Some(&n)));
            };
            construct(vm, ctor, &[])
        }
        "getDeclaredMethods" | "getDeclaredConstructors" | "getMethods" | "getConstructors" => {
            let ms = match op {
                "getDeclaredMethods" => declared_methods(vm, c, false),
                "getDeclaredConstructors" => declared_methods(vm, c, true),
                "getConstructors" => declared_methods(vm, c, true).into_iter().filter(|&m| vm.method(m).access & ACC_PUBLIC != 0).collect(),
                _ => public_methods(vm, c),
            };
            let mut objs = Vec::new();
            for m in ms {
                objs.push(method_object(vm, m)?);
            }
            let elem = if op.contains("Constructor") { "Ljava/lang/reflect/Constructor;" } else { "Ljava/lang/reflect/Method;" };
            ret_ref(vm.new_ref_array(elem, &objs)?)
        }
        "getDeclaredMethod" | "getMethod" => {
            let name = vm.string_nn(arg(a, 1).r())?;
            let params = descs_of(vm, arg(a, 2).r())?;
            let candidates = if op == "getMethod" { public_methods(vm, c) } else { declared_methods(vm, c, false) };
            let found = candidates.into_iter().find(|&m| {
                let method = vm.method(m);
                *method.name == *name && method.desc.starts_with(&format!("({params})"))
            });
            match found {
                Some(m) => ret_ref(method_object(vm, m)?),
                None => {
                    let ps = params_text(vm, &params)?;
                    let msg = format!("{}.{name}({ps})", vm.class(c).java_name());
                    Err(vm.throw_new("java/lang/NoSuchMethodException", Some(&msg)))
                }
            }
        }
        "getDeclaredConstructor" | "getConstructor" => {
            let params = descs_of(vm, arg(a, 1).r())?;
            let found = declared_methods(vm, c, true).into_iter().find(|&m| vm.method(m).desc.starts_with(&format!("({params})")) && (op == "getDeclaredConstructor" || vm.method(m).access & ACC_PUBLIC != 0));
            match found {
                Some(m) => ret_ref(method_object(vm, m)?),
                None => {
                    let ps = params_text(vm, &params)?;
                    let msg = format!("{}.<init>({ps})", vm.class(c).java_name());
                    Err(vm.throw_new("java/lang/NoSuchMethodException", Some(&msg)))
                }
            }
        }
        "getDeclaredFields" | "getFields" => {
            let fields = if op == "getFields" { public_fields(vm, c) } else { all_fields(vm, c) };
            let mut objs = Vec::new();
            for (owner, f, is_static) in fields {
                objs.push(field_object(vm, owner, &f, is_static)?);
            }
            ret_ref(vm.new_ref_array("Ljava/lang/reflect/Field;", &objs)?)
        }
        "getDeclaredField" | "getField" => {
            let name = vm.string_nn(arg(a, 1).r())?;
            let fields = if op == "getField" { public_fields(vm, c) } else { all_fields(vm, c) };
            match fields.into_iter().find(|(_, f, _)| *f.name == *name) {
                Some((owner, f, is_static)) => ret_ref(field_object(vm, owner, &f, is_static)?),
                None => Err(vm.throw_new("java/lang/NoSuchFieldException", Some(&name))),
            }
        }
        "getRecordComponents" => {
            let Some(rec) = vm.class(c).class_file().and_then(|cf| cf.record.clone()) else { return ret_ref(0) };
            if !is_record(vm, c) {
                return ret_ref(0);
            }
            let mut objs = Vec::new();
            for i in 0..rec.len() {
                objs.push(new_member(vm, "java/lang/reflect/RecordComponent", Member::RecordComponent { record: c, index: i })?);
            }
            ret_ref(vm.new_ref_array("Ljava/lang/reflect/RecordComponent;", &objs)?)
        }
        "getEnclosingClass" | "getDeclaringClass" | "getNestHost" => {
            let outer = match vm.class(c).class_file() {
                Some(cf) => {
                    let from_inner = inner_entry(vm, c).and_then(|(o, _)| o);
                    let enclosing = cf.enclosing.and_then(|(cls, _)| cf.class_name(cls).ok().map(|s| s.to_string()));
                    let host = cf.nest_host.and_then(|h| cf.class_name(h).ok().map(|s| s.to_string()));
                    match op {
                        "getDeclaringClass" => from_inner,
                        "getEnclosingClass" => from_inner.or(enclosing),
                        _ => host,
                    }
                }
                None => None,
            };
            match outer {
                Some(n) => {
                    let k = vm.load(&n)?;
                    ret_ref(vm.mirror(k)?)
                }
                None if op == "getNestHost" => ret_ref(t),
                None => ret_ref(0),
            }
        }
        "getDeclaredClasses" | "getClasses" => {
            let Some(cf) = vm.class(c).class_file().cloned() else { return mirror_array(vm, &[]) };
            let mut out = Vec::new();
            for ic in &cf.inner_classes {
                if ic.outer != 0 && cf.class_name(ic.outer).ok().as_deref() == Some(&*cf.name) && (op == "getDeclaredClasses" || ic.access & ACC_PUBLIC != 0) {
                    let n = cf.class_name(ic.inner).map_err(|e| Exc::Internal(e.0))?;
                    out.push(vm.load(&n)?);
                }
            }
            mirror_array(vm, &out)
        }
        "getResourceAsStream" => {
            let name = vm.string_nn(arg(a, 1).r())?;
            let path = if let Some(abs) = name.strip_prefix('/') {
                abs.to_string()
            } else {
                let pkg = vm.class(c).name.rsplit_once('/').map_or(String::new(), |(p, _)| format!("{p}/"));
                format!("{pkg}{name}")
            };
            resource_stream(vm, &path)
        }
        "getResource" => ret_ref(0),
        "getAnnotation" | "getDeclaredAnnotation" | "isAnnotationPresent" => {
            let wanted = mirror_class(vm, arg(a, 1).r())?;
            let found = annotation_of(vm, ElementOf::Class(c), wanted)?;
            if op == "isAnnotationPresent" { ret_bool(found != 0) } else { ret_ref(found) }
        }
        "getAnnotations" | "getDeclaredAnnotations" => annotations_array(vm, ElementOf::Class(c)),
        "getAnnotationsByType" => {
            let wanted = mirror_class(vm, arg(a, 1).r())?;
            let found = annotation_of(vm, ElementOf::Class(c), wanted)?;
            let d = vm.descriptor_of(wanted);
            let items: Vec<ObjId> = if found != 0 { vec![found] } else { Vec::new() };
            ret_ref(vm.new_ref_array(&d, &items)?)
        }
        "getGenericSuperclass" => class_op(vm, a, "getSuperclass"),
        "getGenericInterfaces" => class_op(vm, a, "getInterfaces"),
        "getTypeParameters" => ret_ref(vm.new_ref_array("Ljava/lang/reflect/TypeVariable;", &[])?),
        "getEnclosingMethod" | "getEnclosingConstructor" | "getSigners" | "getProtectionDomain" | "getModule" => ret_ref(0),
        "isNestmateOf" => {
            let other = mirror_class(vm, arg(a, 1).r())?;
            let host = |vm: &mut Vm, k: ClassId| -> Res<ObjId> {
                let m = vm.mirror(k)?;
                Ok(class_op(vm, &[Value::Ref(m)], "getNestHost")?.r())
            };
            let (x, y) = (host(vm, c)?, host(vm, other)?);
            ret_bool(x == y)
        }
        "hashCode" => Ok(Value::Int(vm.identity_hash(t))),
        _ => Err(Exc::Unsupported(format!("Class.{op}"))),
    }
}

fn params_text(vm: &mut Vm, descs: &str) -> Res<String> {
    let _ = vm;
    let full = format!("({descs})V");
    let (params, _) = classfile::parse_method_desc(&full).unwrap_or_default();
    Ok(params.iter().map(|p| desc_to_java(p)).collect::<Vec<_>>().join(", "))
}

fn resource_stream(vm: &mut Vm, path: &str) -> Res<Value> {
    match vm.source.resource(path) {
        Some(bytes) => ret_ref(super::io::new_input_stream(vm, bytes.to_vec())?),
        None => ret_ref(0),
    }
}

fn app_loader(vm: &mut Vm) -> Res<ObjId> {
    let c = vm.load("jdk/internal/loader/ClassLoaders$AppClassLoader")?;
    if let Some(Value::Ref(r)) = vm.class(c).statics.first().copied()
        && r != 0
    {
        return Ok(r);
    }
    let r = vm.new_instance(c)?;
    vm.pinned.push(r);
    if let Some(s) = vm.classes[c as usize].statics.first_mut() {
        *s = Value::Ref(r);
    }
    Ok(r)
}

fn loader_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "getSystemClassLoader" | "getPlatformClassLoader" => ret_ref(app_loader(vm)?),
        "loadClass" => for_name(vm, arg(a, 1).r(), false),
        "getResourceAsStream" => {
            let name = vm.string_nn(arg(a, 1).r())?;
            resource_stream(vm, name.trim_start_matches('/'))
        }
        "getName" => ret_str(vm, "app"),
        "getParent" | "getResource" => ret_ref(0),
        "toString" => ret_str(vm, "jdk.internal.loader.ClassLoaders$AppClassLoader@1"),
        _ => Err(Exc::Unsupported(format!("ClassLoader.{op}"))),
    }
}

// ------------------------------------------------------------------ members

/// The text Java's `Modifier.toString` makes.
pub fn modifier_text(m: i32) -> String {
    let names = [
        (0x0001, "public"),
        (0x0004, "protected"),
        (0x0002, "private"),
        (0x0400, "abstract"),
        (0x0008, "static"),
        (0x0010, "final"),
        (0x0080, "transient"),
        (0x0040, "volatile"),
        (0x0020, "synchronized"),
        (0x0100, "native"),
        (0x0800, "strictfp"),
        (0x0200, "interface"),
    ];
    names.iter().filter(|(bit, _)| m & bit != 0).map(|(_, n)| *n).collect::<Vec<_>>().join(" ")
}

fn method_modifiers(vm: &Vm, m: MethodId) -> i32 {
    (vm.method(m).access & 0x0D3F) as i32
}

/// `Method.toString` and `Constructor.toString`.
fn executable_text(vm: &Vm, m: MethodId) -> String {
    let method = vm.method(m);
    let mut mods = method_modifiers(vm, m);
    let owner = vm.class(method.class);
    if owner.is_interface() && !method.is_abstract() && !method.is_static() && method.access & ACC_PRIVATE == 0 {
        mods &= !(ACC_ABSTRACT as i32);
    }
    let mut text = modifier_text(mods & !(ACC_SYNTHETIC as i32));
    if owner.is_interface() && !method.is_abstract() && !method.is_static() && method.access & ACC_PRIVATE == 0 {
        text = if text.is_empty() { "default".into() } else { format!("{text} default") };
    }
    let (params, ret) = classfile::parse_method_desc(&method.desc).unwrap_or_default();
    let params = params.iter().map(|p| desc_to_java(p)).collect::<Vec<_>>().join(",");
    let name = owner.java_name();
    let mut out = if text.is_empty() { String::new() } else { format!("{text} ") };
    if &*method.name == "<init>" {
        out.push_str(&format!("{name}({params})"));
    } else {
        out.push_str(&format!("{} {name}.{}({params})", desc_to_java(ret), method.name));
    }
    let throws = exception_names(vm, m);
    if !throws.is_empty() {
        out.push_str(&format!(" throws {}", throws.join(",")));
    }
    out
}

fn exception_names(vm: &Vm, m: MethodId) -> Vec<String> {
    let method = vm.method(m);
    let (Some(info), Some(cf)) = (&method.info, vm.class(method.class).class_file()) else { return Vec::new() };
    info.exceptions.iter().filter_map(|&i| cf.class_name(i).ok()).map(|n| n.replace('/', ".")).collect()
}

/// Whether reflective access from the calling class needs `setAccessible`.
fn access_denied(vm: &Vm, owner: ClassId, access: u16, accessible: bool) -> bool {
    if accessible || access & ACC_PUBLIC != 0 && vm.class(owner).access & ACC_PUBLIC != 0 {
        return false;
    }
    let caller = super::invoke::caller_class(vm);
    if caller == owner {
        return false;
    }
    let pkg = |c: ClassId| vm.class(c).name.rsplit_once('/').map_or(String::new(), |(p, _)| p.to_string());
    if access & ACC_PRIVATE != 0 {
        // Nestmates may.
        let host = |c: ClassId| vm.class(c).class_file().and_then(|cf| cf.nest_host.and_then(|h| cf.class_name(h).ok().map(|s| s.to_string()))).unwrap_or_else(|| vm.class(c).name.to_string());
        return host(caller) != host(owner);
    }
    if access & ACC_PUBLIC != 0 {
        return false;
    }
    if access & ACC_PROTECTED != 0 && vm.is_subclass(caller, owner) {
        return false;
    }
    pkg(caller) != pkg(owner)
}

fn illegal_access(vm: &mut Vm, owner: ClassId, access: u16) -> Exc {
    let caller = super::invoke::caller_class(vm);
    let mods = modifier_text((access & 0x0D3F) as i32);
    let msg = format!(
        "class {} cannot access a member of class {} with modifiers \"{mods}\"",
        vm.class(caller).java_name(),
        vm.class(owner).java_name()
    );
    vm.throw_new("java/lang/IllegalAccessException", Some(&msg))
}

/// Arguments of a reflective call, unboxed for their parameters.
fn call_args(vm: &mut Vm, desc: &str, arr: ObjId) -> Res<Vec<Value>> {
    let (params, _) = classfile::parse_method_desc(desc).unwrap_or_default();
    let params: Vec<String> = params.iter().map(|s| s.to_string()).collect();
    let given = if arr == 0 { Vec::new() } else { vm.ref_elements(arr) };
    if given.len() != params.len() {
        let msg = format!("wrong number of arguments: {} expected: {}", given.len(), params.len());
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
    }
    let mut out = Vec::new();
    for (v, p) in given.into_iter().zip(&params) {
        if p.len() == 1 {
            if v == 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", None));
            }
            let ok = matches!(vm.obj(v).map(|o| &o.data), Some(Data::Boxed(_)));
            if !ok {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("argument type mismatch")));
            }
            out.push(super::lang::unbox(vm, v, p.as_bytes()[0])?);
        } else {
            if v != 0 {
                let c = vm.class_of_desc(p)?;
                if !vm.instance_of(v, c) {
                    return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("argument type mismatch")));
                }
            }
            out.push(Value::Ref(v));
        }
    }
    Ok(out)
}

/// An exception a reflectively called method threw, wrapped as Java wraps it.
fn wrap_target(vm: &mut Vm, e: Exc) -> Exc {
    match e {
        Exc::Throw(ex) => match vm.make_throwable("java/lang/reflect/InvocationTargetException", None) {
            Ok(w) => {
                if let Some(Data::Throwable(t)) = vm.obj_mut(w).map(|o| &mut o.data) {
                    t.cause = Some(ex);
                }
                Exc::Throw(w)
            }
            Err(e) => e,
        },
        other => other,
    }
}

fn construct(vm: &mut Vm, ctor: MethodId, args: &[Value]) -> Res<Value> {
    let class = vm.method(ctor).class;
    if vm.class(class).access & (ACC_ABSTRACT | ACC_INTERFACE) != 0 {
        return Err(vm.throw_new("java/lang/InstantiationException", None));
    }
    vm.ensure_init(class)?;
    let r = vm.new_instance(class)?;
    let mut call = vec![Value::Ref(r)];
    call.extend_from_slice(args);
    vm.invoke(ctor, &call).map_err(|e| wrap_target(vm, e))?;
    ret_ref(r)
}

fn box_result(vm: &mut Vm, v: Value, ret: &str) -> Res<Value> {
    match ret.as_bytes()[0] {
        b'V' => ret_ref(0),
        b'L' | b'[' => Ok(v),
        k => ret_ref(super::lang::box_value(vm, v, k)?),
    }
}

fn executable_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(rf) = reflected(vm, t) else { return Err(Exc::Internal("not a member".into())) };
    let (Member::Method(m) | Member::Constructor(m)) = rf.member else { return Err(Exc::Internal("not an executable".into())) };
    let method = vm.method(m).clone();
    match op {
        "getName" => {
            let n = if &*method.name == "<init>" { vm.class(method.class).java_name() } else { method.name.to_string() };
            ret_ref(vm.intern(&n)?)
        }
        "getDeclaringClass" => ret_ref(vm.mirror(method.class)?),
        "getModifiers" => Ok(Value::Int(method_modifiers(vm, m))),
        "getParameterTypes" => {
            let ps = param_classes(vm, &method.desc)?;
            mirror_array(vm, &ps)
        }
        "getParameterCount" => Ok(Value::Int(param_classes(vm, &method.desc)?.len() as i32)),
        "getReturnType" | "getGenericReturnType" => {
            let (_, ret) = classfile::parse_method_desc(&method.desc).unwrap_or_default();
            let ret = ret.to_string();
            let c = vm.class_of_desc(&ret)?;
            ret_ref(vm.mirror(c)?)
        }
        "getExceptionTypes" => {
            let names = exception_names(vm, m);
            let mut cs = Vec::new();
            for n in names {
                cs.push(vm.load(&n.replace('.', "/"))?);
            }
            mirror_array(vm, &cs)
        }
        "getParameters" => {
            let n = param_classes(vm, &method.desc)?.len();
            let mut objs = Vec::new();
            for i in 0..n {
                objs.push(new_member(vm, "java/lang/reflect/Parameter", Member::Parameter { method: m, index: i })?);
            }
            ret_ref(vm.new_ref_array("Ljava/lang/reflect/Parameter;", &objs)?)
        }
        "isVarArgs" => ret_bool(method.access & ACC_VARARGS != 0),
        "isSynthetic" => ret_bool(method.access & ACC_SYNTHETIC != 0),
        "isBridge" => ret_bool(method.access & 0x40 != 0),
        "isDefault" => ret_bool(vm.class(method.class).is_interface() && !method.is_abstract() && !method.is_static() && method.access & ACC_PUBLIC != 0),
        "toString" | "toGenericString" => {
            let s = executable_text(vm, m);
            ret_str(vm, &s)
        }
        "invoke" => {
            if access_denied(vm, method.class, method.access, rf.accessible) {
                return Err(illegal_access(vm, method.class, method.access));
            }
            let args = call_args(vm, &method.desc, arg(a, 2).r())?;
            let (_, ret) = classfile::parse_method_desc(&method.desc).unwrap_or_default();
            let ret = ret.to_string();
            let result = if method.is_static() {
                vm.ensure_init(method.class)?;
                vm.invoke(m, &args).map_err(|e| wrap_target(vm, e))?
            } else {
                let recv = arg(a, 1).r();
                if recv == 0 {
                    let msg = format!("Cannot invoke \"Object.getClass()\" because \"obj\" is null");
                    let _ = msg;
                    return Err(vm.npe(None));
                }
                if !vm.instance_of(recv, method.class) {
                    return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("object is not an instance of declaring class")));
                }
                let target = if method.access & ACC_PRIVATE != 0 {
                    m
                } else {
                    let c = vm.class_of(recv).unwrap();
                    vm.select(c, method.nd)?
                };
                let mut call = vec![Value::Ref(recv)];
                call.extend(args);
                vm.invoke(target, &call).map_err(|e| wrap_target(vm, e))?
            };
            box_result(vm, result, &ret)
        }
        "newInstance" => {
            if access_denied(vm, method.class, method.access, rf.accessible) {
                return Err(illegal_access(vm, method.class, method.access));
            }
            let args = call_args(vm, &method.desc, arg(a, 1).r())?;
            construct(vm, m, &args)
        }
        "getAnnotation" | "getDeclaredAnnotation" | "isAnnotationPresent" => {
            let wanted = mirror_class(vm, arg(a, 1).r())?;
            let found = annotation_of(vm, ElementOf::Method(m), wanted)?;
            if op == "isAnnotationPresent" { ret_bool(found != 0) } else { ret_ref(found) }
        }
        "getAnnotations" | "getDeclaredAnnotations" => annotations_array(vm, ElementOf::Method(m)),
        "getDefaultValue" => {
            let Some(bytes) = method.info.as_ref().and_then(|i| i.annotation_default.clone()) else { return ret_ref(0) };
            let Some(cf) = vm.class(method.class).class_file().cloned() else { return ret_ref(0) };
            let (_, ret) = classfile::parse_method_desc(&method.desc).unwrap_or_default();
            let ret = ret.to_string();
            let mut r = Reader::new(&bytes);
            let v = element_value(vm, &cf, &mut r, &ret)?;
            box_result(vm, v, &ret)
        }
        _ => common_op(vm, a, op),
    }
}

fn common_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "setAccessible" => {
            if let Some(State::Reflect(r)) = state_mut(vm, t) {
                r.accessible = arg(a, 1).z();
            }
            void()
        }
        "trySetAccessible" => {
            if let Some(State::Reflect(r)) = state_mut(vm, t) {
                r.accessible = true;
            }
            ret_bool(true)
        }
        "isAccessible" | "canAccess" => ret_bool(reflected(vm, t).is_some_and(|r| r.accessible)),
        "equals" => {
            let (x, y) = (member(vm, t), member(vm, arg(a, 1).r()));
            let same = match (x, y) {
                (Some(Member::Method(p)), Some(Member::Method(q))) | (Some(Member::Constructor(p)), Some(Member::Constructor(q))) => p == q,
                (Some(Member::Field { owner: o1, slot: s1, is_static: i1, .. }), Some(Member::Field { owner: o2, slot: s2, is_static: i2, .. })) => o1 == o2 && s1 == s2 && i1 == i2,
                _ => false,
            };
            ret_bool(same)
        }
        "hashCode" => {
            let h = match member(vm, t) {
                Some(Member::Method(m) | Member::Constructor(m)) => {
                    let method = vm.method(m).clone();
                    let cn: Vec<u16> = vm.class(method.class).java_name().encode_utf16().collect();
                    let n: Vec<u16> = method.name.encode_utf16().collect();
                    super::string::java_hash(&cn) ^ super::string::java_hash(&n)
                }
                Some(Member::Field { owner, name, .. }) => {
                    let cn: Vec<u16> = vm.class(owner).java_name().encode_utf16().collect();
                    let n: Vec<u16> = name.encode_utf16().collect();
                    super::string::java_hash(&cn) ^ super::string::java_hash(&n)
                }
                _ => 0,
            };
            Ok(Value::Int(h))
        }
        _ => Err(Exc::Unsupported(format!("reflection {op}"))),
    }
}

fn field_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(rf) = reflected(vm, t) else { return Err(Exc::Internal("not a member".into())) };
    let Member::Field { owner, name, desc, slot, is_static, access, .. } = rf.member.clone() else { return common_op(vm, a, op) };
    match op {
        "getName" => ret_ref(vm.intern(&name)?),
        "getDeclaringClass" => ret_ref(vm.mirror(owner)?),
        "getModifiers" => Ok(Value::Int((access & 0x50DF) as i32)),
        "getType" | "getGenericType" => {
            let c = vm.class_of_desc(&desc)?;
            ret_ref(vm.mirror(c)?)
        }
        "isEnumConstant" => ret_bool(access & ACC_ENUM != 0),
        "isSynthetic" => ret_bool(access & ACC_SYNTHETIC != 0),
        "toString" | "toGenericString" => {
            let mods = modifier_text((access & 0x00DF) as i32);
            let s = format!("{}{} {}.{name}", if mods.is_empty() { String::new() } else { format!("{mods} ") }, desc_to_java(&desc), vm.class(owner).java_name());
            ret_str(vm, &s)
        }
        "getAnnotation" | "getDeclaredAnnotation" | "isAnnotationPresent" => {
            let wanted = mirror_class(vm, arg(a, 1).r())?;
            let found = annotation_of(vm, ElementOf::Field(t), wanted)?;
            if op == "isAnnotationPresent" { ret_bool(found != 0) } else { ret_ref(found) }
        }
        "getAnnotations" | "getDeclaredAnnotations" => annotations_array(vm, ElementOf::Field(t)),
        _ if op.starts_with("get") || op.starts_with("set") => {
            if access_denied(vm, owner, access, rf.accessible) {
                return Err(illegal_access(vm, owner, access));
            }
            let obj = arg(a, 1).r();
            if !is_static {
                if obj == 0 {
                    return Err(vm.npe(None));
                }
                if !vm.instance_of(obj, owner) {
                    let msg = format!("Can not {} {} field {}.{name} to {}", if op.starts_with("get") { "get" } else { "set" }, desc_to_java(&desc), vm.class(owner).java_name(), vm.class_of(obj).map(|c| vm.class(c).java_name()).unwrap_or_default());
                    return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
                }
            } else {
                vm.ensure_init(owner)?;
            }
            if op.starts_with("get") {
                let v = if is_static { vm.class(owner).statics[slot] } else { vm.obj(obj).map(|o| o.fields[slot]).unwrap_or_default() };
                if op == "get" {
                    return box_result(vm, v, &desc);
                }
                // getInt, getLong...: widening as Java allows.
                let want = op.as_bytes()[3];
                let kind = match want {
                    b'B' if op == "getBoolean" => b'Z',
                    b'B' => b'B',
                    b'C' => b'C',
                    b'S' => b'S',
                    b'I' => b'I',
                    b'L' => b'J',
                    b'F' => b'F',
                    _ => b'D',
                };
                if desc.len() != 1 {
                    return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Attempt to get a non-primitive field as a primitive")));
                }
                return Ok(super::lang::widen(v, kind));
            }
            if access & ACC_FINAL != 0 && (is_static || !rf.accessible) {
                let mods = modifier_text((access & 0x00DF) as i32);
                let msg = format!("Can not set {mods} {} field {}.{name} to {}", desc_to_java(&desc), vm.class(owner).java_name(), "java.lang.Object");
                return Err(vm.throw_new("java/lang/IllegalAccessException", Some(&msg)));
            }
            let raw = arg(a, 2);
            let v = if op == "set" {
                if desc.len() == 1 {
                    if raw.r() == 0 {
                        return Err(vm.throw_new("java/lang/IllegalArgumentException", None));
                    }
                    super::lang::unbox(vm, raw.r(), desc.as_bytes()[0])?
                } else {
                    if raw.r() != 0 {
                        let c = vm.class_of_desc(&desc)?;
                        if !vm.instance_of(raw.r(), c) {
                            let from = vm.class_of(raw.r()).map(|k| vm.class(k).java_name()).unwrap_or_default();
                            let msg = format!("Can not set {} field {}.{name} to {from}", desc_to_java(&desc), vm.class(owner).java_name());
                            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
                        }
                    }
                    raw
                }
            } else {
                super::lang::widen(raw, desc.as_bytes()[0])
            };
            if is_static {
                vm.classes[owner as usize].statics[slot] = v;
            } else if let Some(o) = vm.obj_mut(obj) {
                o.fields[slot] = v;
            }
            void()
        }
        _ => common_op(vm, a, op),
    }
}

fn record_component_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(Member::RecordComponent { record, index }) = member(vm, t) else { return Err(Exc::Internal("not a record component".into())) };
    let comp = vm.class(record).class_file().and_then(|cf| cf.record.as_ref().and_then(|r| r.get(index).cloned())).ok_or_else(|| Exc::Internal("record component".into()))?;
    match op {
        "getName" => ret_ref(vm.intern(&comp.name)?),
        "getType" | "getGenericType" => {
            let c = vm.class_of_desc(&comp.desc)?;
            ret_ref(vm.mirror(c)?)
        }
        "getAccessor" => {
            let d = format!("(){}", comp.desc);
            match vm.find_method(record, &comp.name, &d) {
                Some(m) => ret_ref(method_object(vm, m)?),
                None => ret_ref(0),
            }
        }
        "getDeclaringRecord" => ret_ref(vm.mirror(record)?),
        "toString" => {
            let s = format!("{} {}", desc_to_java(&comp.desc), comp.name);
            ret_str(vm, &s)
        }
        _ => Err(Exc::Unsupported(format!("RecordComponent.{op}"))),
    }
}

fn parameter_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(Member::Parameter { method, index }) = member(vm, t) else { return Err(Exc::Internal("not a parameter".into())) };
    let m = vm.method(method).clone();
    let (params, _) = classfile::parse_method_desc(&m.desc).unwrap_or_default();
    let desc = params.get(index).map(|s| s.to_string()).unwrap_or_default();
    let name = m.info.as_ref().and_then(|i| i.parameter_names.get(index).cloned().flatten()).map(|s| s.to_string());
    match op {
        "getName" => {
            let n = name.unwrap_or_else(|| format!("arg{index}"));
            ret_str(vm, &n)
        }
        "isNamePresent" => ret_bool(name.is_some()),
        "getType" | "getParameterizedType" => {
            let c = vm.class_of_desc(&desc)?;
            ret_ref(vm.mirror(c)?)
        }
        "getModifiers" => Ok(Value::Int(0)),
        "toString" => {
            let n = name.unwrap_or_else(|| format!("arg{index}"));
            let s = format!("{} {n}", desc_to_java(&desc));
            ret_str(vm, &s)
        }
        _ => Err(Exc::Unsupported(format!("Parameter.{op}"))),
    }
}

// ------------------------------------------------------------------ Array

fn array_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "newInstance" => {
            let comp = mirror_class(vm, arg(a, 0).r())?;
            if vm.class_name(comp) == "V" || matches!(vm.class(comp).kind, ClassKind::Primitive(b'V')) {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", None));
            }
            let n = arg(a, 1).i();
            if n < 0 {
                return Err(vm.throw_new("java/lang/NegativeArraySizeException", Some(&n.to_string())));
            }
            let ac = vm.array_class_of(comp)?;
            ret_ref(vm.new_array(ac, n)?)
        }
        "newInstanceDims" => {
            let comp = mirror_class(vm, arg(a, 0).r())?;
            let dims: Vec<i32> = vm.array_values(arg(a, 1).r()).iter().map(|v| v.i()).collect();
            if dims.is_empty() {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Empty dimensions array")));
            }
            let mut class = comp;
            for _ in 0..dims.len() {
                class = vm.array_class_of(class)?;
            }
            ret_ref(multi_array(vm, class, &dims)?)
        }
        "getLength" => match vm.array(arg(a, 0).r()) {
            Some(arr) => Ok(Value::Int(arr.len() as i32)),
            None if arg(a, 0).r() == 0 => Err(vm.npe(None)),
            None => Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Argument is not an array"))),
        },
        "get" | "getInt" | "getLong" | "getDouble" | "getBoolean" | "getChar" | "getByte" | "getShort" | "getFloat" => {
            let (arr, i) = (arg(a, 0).r(), arg(a, 1).i());
            let Some(len) = vm.array(arr).map(Array::len) else {
                if arr == 0 {
                    return Err(vm.npe(None));
                }
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Argument is not an array")));
            };
            if i < 0 || i as usize >= len {
                let msg = format!("Index {i} out of bounds for length {len}");
                return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", Some(&msg)));
            }
            let class = vm.class_of(arr).unwrap();
            let elem = match vm.class(class).kind {
                ClassKind::Array { elem, .. } => elem,
                _ => b'L',
            };
            let v = vm.array(arr).and_then(|x| x.get(i as usize)).unwrap_or_default();
            if op == "get" {
                return box_result(vm, v, &(elem as char).to_string());
            }
            let kind = match op {
                "getInt" => b'I',
                "getLong" => b'J',
                "getDouble" => b'D',
                "getBoolean" => b'Z',
                "getChar" => b'C',
                "getByte" => b'B',
                "getShort" => b'S',
                _ => b'F',
            };
            Ok(super::lang::widen(v, kind))
        }
        _ => {
            // set and setInt...: unboxed and widened to the element type.
            let (arr, i, v) = (arg(a, 0).r(), arg(a, 1).i(), arg(a, 2));
            let Some(class) = vm.class_of(arr) else { return Err(vm.npe(None)) };
            let elem = match vm.class(class).kind {
                ClassKind::Array { elem, .. } => elem,
                _ => return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Argument is not an array"))),
            };
            let value = match (op, elem) {
                ("set", b'L') => v,
                ("set", p) => {
                    if v.r() == 0 {
                        return Err(vm.throw_new("java/lang/IllegalArgumentException", None));
                    }
                    super::lang::unbox(vm, v.r(), p)?
                }
                (_, p) => super::lang::widen(v, p),
            };
            super::lang::array_store(vm, arr, i.max(0) as usize, value)?;
            void()
        }
    }
}

fn multi_array(vm: &mut Vm, class: ClassId, dims: &[i32]) -> Res<ObjId> {
    let n = dims[0];
    if n < 0 {
        return Err(vm.throw_new("java/lang/NegativeArraySizeException", Some(&n.to_string())));
    }
    let r = vm.new_array(class, n)?;
    if dims.len() > 1
        && let ClassKind::Array { component, .. } = vm.class(class).kind
    {
        for i in 0..n as usize {
            let inner = multi_array(vm, component, &dims[1..])?;
            vm.ref_array_set(r, i, inner);
        }
    }
    Ok(r)
}

// ------------------------------------------------------------------ proxies

fn proxy_new(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let interfaces: Vec<ClassId> = {
        let arr = arg(a, 1).r();
        if arr == 0 {
            return Err(vm.npe(None));
        }
        let mut out = Vec::new();
        for m in vm.ref_elements(arr) {
            out.push(mirror_class(vm, m)?);
        }
        out
    };
    let handler = arg(a, 2).r();
    if handler == 0 {
        return Err(vm.npe(None));
    }
    for &i in &interfaces {
        if !vm.class(i).is_interface() {
            let msg = format!("{} is not an interface", vm.class(i).java_name());
            return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
        }
    }
    let index = proxy_class(vm, &interfaces, None)?;
    let class = vm.jdk.proxies[index].class;
    let r = vm.new_instance(class)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(handler)])));
    }
    ret_ref(r)
}

/// The proxy class for these interfaces, made on first use.
fn proxy_class(vm: &mut Vm, interfaces: &[ClassId], annotation: Option<ClassId>) -> Res<usize> {
    if let Some(i) = vm.jdk.proxies.iter().position(|p| p.interfaces == interfaces && p.annotation == annotation) {
        return Ok(i);
    }
    let index = vm.jdk.proxies.len();
    let n = vm.jdk.proxies.iter().filter(|p| p.annotation.is_none()).count();
    let package = interfaces
        .iter()
        .find(|&&i| vm.class(i).access & ACC_PUBLIC == 0)
        .and_then(|&i| vm.class(i).name.rsplit_once('/').map(|(p, _)| p.to_string()))
        .unwrap_or_else(|| "jdk/proxy1".into());
    let name = if annotation.is_some() { format!("jdk/proxy2/$Proxy{}", 100 + index) } else { format!("{package}/$Proxy{n}") };
    let proxy_super = vm.load("java/lang/reflect/Proxy")?;
    let id = vm.classes.len() as ClassId;
    let mut class = vm.class(proxy_super).clone();
    class.id = id;
    class.name = name.as_str().into();
    class.access = ACC_PUBLIC | ACC_FINAL;
    class.kind = ClassKind::Synthetic;
    class.super_class = Some(proxy_super);
    class.interfaces = interfaces.to_vec();
    class.methods = Default::default();
    class.method_order = Vec::new();
    class.selected = Default::default();
    class.state = InitState::Initialized;
    class.mirror = 0;
    class.synthetic = Some(index);
    vm.classes.push(class);
    vm.class_by_name.insert(name.as_str().into(), id);
    // Every interface method (and Object's three) calls the handler.
    let mut methods: Vec<(String, String)> = vec![
        ("hashCode".into(), "()I".into()),
        ("equals".into(), "(Ljava/lang/Object;)Z".into()),
        ("toString".into(), "()Ljava/lang/String;".into()),
    ];
    let mut queue: Vec<ClassId> = interfaces.to_vec();
    let mut seen = Vec::new();
    while let Some(i) = queue.pop() {
        if seen.contains(&i) {
            continue;
        }
        seen.push(i);
        for &m in &vm.class(i).method_order.clone() {
            let method = vm.method(m);
            if method.is_static() || method.access & ACC_PRIVATE != 0 || &*method.name == "<clinit>" {
                continue;
            }
            let key = (method.name.to_string(), method.desc.to_string());
            if !methods.contains(&key) {
                methods.push(key);
            }
        }
        queue.extend(vm.class(i).interfaces.clone());
    }
    for (n, d) in &methods {
        vm.add_method(id, n, d, ACC_PUBLIC | ACC_FINAL, MethodBody::Proxy, None)?;
    }
    vm.jdk.proxies.push(ProxyClass { class: id, interfaces: interfaces.to_vec(), methods: HashMap::new(), annotation });
    Ok(index)
}

/// A proxy's method: its handler's `invoke(proxy, method, args)`, or an
/// annotation's element.
pub fn call_proxy(vm: &mut Vm, m: MethodId, args: &[Value]) -> Res<Value> {
    let proxy = args.first().map_or(0, |v| v.r());
    let Some(class) = vm.class_of(proxy) else { return Err(vm.npe(None)) };
    let Some(index) = vm.class(class).synthetic else { return Err(Exc::Internal("proxy method of a non-proxy".into())) };
    let method = vm.method(m).clone();
    if let Some(ann) = vm.jdk.proxies[index].annotation {
        return annotation_call(vm, proxy, ann, &method.name, &method.desc, args);
    }
    let handler = match state(vm, proxy) {
        Some(State::Generic(v)) => v[0].r(),
        _ => 0,
    };
    let method_obj = match vm.jdk.proxies[index].methods.get(&method.nd) {
        Some(&o) => o,
        None => {
            let interfaces = vm.jdk.proxies[index].interfaces.clone();
            let mut target = None;
            for i in interfaces {
                if let Some(t) = vm.resolve_method(i, method.nd) {
                    target = Some(t);
                    break;
                }
            }
            let target = match target {
                Some(t) => t,
                None => {
                    let object = vm.known.object;
                    vm.resolve_method(object, method.nd).ok_or_else(|| Exc::Internal("proxy method target".into()))?
                }
            };
            let o = method_object(vm, target)?;
            vm.pinned.push(o);
            vm.jdk.proxies[index].methods.insert(method.nd, o);
            o
        }
    };
    let (params, ret) = classfile::parse_method_desc(&method.desc).map(|(p, r)| (p.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.to_string())).unwrap_or_default();
    let boxed = if params.is_empty() {
        0
    } else {
        let mut items = Vec::new();
        for (v, p) in args[1..].iter().zip(&params) {
            items.push(vm.adapt(*v, if p.len() == 1 { "Ljava/lang/Object;" } else { p })?.r());
        }
        vm.new_ref_array("Ljava/lang/Object;", &items)?
    };
    let result = vm.invoke_virtual(handler, "invoke", "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;", &[Value::Ref(proxy), Value::Ref(method_obj), Value::Ref(boxed)]);
    let result = match result {
        Err(Exc::Throw(ex)) => {
            // A checked exception the method does not declare is wrapped.
            let runtime = vm.load("java/lang/RuntimeException")?;
            let error = vm.load("java/lang/Error")?;
            if vm.instance_of(ex, runtime) || vm.instance_of(ex, error) {
                return Err(Exc::Throw(ex));
            }
            let declared = exception_names(vm, vm.jdk.proxies[index].methods.get(&method.nd).and_then(|o| member(vm, *o)).and_then(|mm| if let Member::Method(x) = mm { Some(x) } else { None }).unwrap_or(m));
            let mut ok = false;
            for d in declared {
                let dc = vm.load(&d.replace('.', "/"))?;
                if vm.instance_of(ex, dc) {
                    ok = true;
                }
            }
            if ok {
                return Err(Exc::Throw(ex));
            }
            let w = vm.make_throwable("java/lang/reflect/UndeclaredThrowableException", None)?;
            if let Some(Data::Throwable(t)) = vm.obj_mut(w).map(|o| &mut o.data) {
                t.cause = Some(ex);
            }
            return Err(Exc::Throw(w));
        }
        other => other?,
    };
    if ret.len() == 1 && ret != "V" && result.r() == 0 {
        return Err(vm.npe(None));
    }
    vm.adapt_return(result, "Ljava/lang/Object;", &ret)
}

fn proxy_static(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "isProxyClass" => {
            let c = mirror_class(vm, arg(a, 0).r())?;
            ret_bool(vm.class(c).synthetic.is_some_and(|i| vm.jdk.proxies.get(i).is_some_and(|p| p.annotation.is_none())))
        }
        "getInvocationHandler" => {
            let p = arg(a, 0).r();
            let is_proxy = vm.class_of(p).and_then(|c| vm.class(c).synthetic).is_some();
            if !is_proxy {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("not a proxy instance")));
            }
            match state(vm, p) {
                Some(State::Generic(v)) => Ok(v[0]),
                _ => ret_ref(0),
            }
        }
        _ => Err(Exc::Unsupported(format!("Proxy.{op}"))),
    }
}

// ------------------------------------------------------------------ annotations

#[derive(Clone, Copy)]
enum ElementOf {
    Class(ClassId),
    Method(MethodId),
    /// A `Field` object.
    Field(ObjId),
}

/// The annotation bytes of an element and the class file whose pool they use.
fn annotation_bytes(vm: &Vm, e: ElementOf) -> Option<(Arc<ClassFile>, Arc<[u8]>)> {
    match e {
        ElementOf::Class(c) => {
            let cf = vm.class(c).class_file()?.clone();
            let bytes = cf.annotations.clone()?;
            Some((cf, bytes))
        }
        ElementOf::Method(m) => {
            let method = vm.method(m);
            let cf = vm.class(method.class).class_file()?.clone();
            let bytes = method.info.as_ref()?.annotations.clone()?;
            Some((cf, bytes))
        }
        ElementOf::Field(f) => match member(vm, f)? {
            Member::Field { owner, annotations, .. } => Some((vm.class(owner).class_file()?.clone(), annotations?)),
            _ => None,
        },
    }
}

/// The runtime-visible annotations of an element, made as objects (and
/// those a superclass declares `@Inherited`).
fn annotations(vm: &mut Vm, e: ElementOf) -> Res<Vec<(ClassId, ObjId)>> {
    let mut out = Vec::new();
    if let Some((cf, bytes)) = annotation_bytes(vm, e) {
        let mut r = Reader::new(&bytes);
        let n = r.u2().map_err(|e| Exc::Internal(e.0))?;
        for _ in 0..n {
            if let Some((c, obj)) = annotation_value(vm, &cf, &mut r)? {
                out.push((c, obj));
            }
        }
    }
    if let ElementOf::Class(c) = e
        && let Some(sup) = vm.class(c).super_class
    {
        for (ac, obj) in annotations(vm, ElementOf::Class(sup))? {
            let inherited = annotation_bytes(vm, ElementOf::Class(ac)).is_some() && has_meta(vm, ac, "java/lang/annotation/Inherited")?;
            if inherited && !out.iter().any(|(x, _)| *x == ac) {
                out.push((ac, obj));
            }
        }
    }
    Ok(out)
}

fn has_meta(vm: &mut Vm, annotation: ClassId, meta: &str) -> Res<bool> {
    let Some((cf, bytes)) = annotation_bytes(vm, ElementOf::Class(annotation)) else { return Ok(false) };
    let mut r = Reader::new(&bytes);
    let n = r.u2().map_err(|e| Exc::Internal(e.0))?;
    for _ in 0..n {
        let t = r.u2().map_err(|e| Exc::Internal(e.0))?;
        let desc = cf.utf8(t).map_err(|e| Exc::Internal(e.0))?.text.clone();
        if *desc == *format!("L{meta};") {
            return Ok(true);
        }
        skip_pairs(&mut r)?;
    }
    Ok(false)
}

fn skip_pairs(r: &mut Reader) -> Res<()> {
    let pairs = r.u2().map_err(|e| Exc::Internal(e.0))?;
    for _ in 0..pairs {
        r.u2().map_err(|e| Exc::Internal(e.0))?;
        skip_element(r)?;
    }
    Ok(())
}

fn skip_element(r: &mut Reader) -> Res<()> {
    let tag = r.u1().map_err(|e| Exc::Internal(e.0))?;
    match tag {
        b'e' => {
            r.u2().map_err(|e| Exc::Internal(e.0))?;
            r.u2().map_err(|e| Exc::Internal(e.0))?;
        }
        b'@' => {
            r.u2().map_err(|e| Exc::Internal(e.0))?;
            skip_pairs(r)?;
        }
        b'[' => {
            let n = r.u2().map_err(|e| Exc::Internal(e.0))?;
            for _ in 0..n {
                skip_element(r)?;
            }
        }
        _ => {
            r.u2().map_err(|e| Exc::Internal(e.0))?;
        }
    }
    Ok(())
}

/// One annotation: its type and an object implementing it, or `None` for a
/// type the class path lacks (Java ignores those too).
fn annotation_value(vm: &mut Vm, cf: &ClassFile, r: &mut Reader) -> Res<Option<(ClassId, ObjId)>> {
    let t = r.u2().map_err(|e| Exc::Internal(e.0))?;
    let desc = cf.utf8(t).map_err(|e| Exc::Internal(e.0))?.text.to_string();
    let name = desc.trim_start_matches('L').trim_end_matches(';').to_string();
    let Some(ac) = vm.try_load(&name).ok().flatten() else {
        skip_pairs(r)?;
        return Ok(None);
    };
    let pairs = r.u2().map_err(|e| Exc::Internal(e.0))?;
    let mut values: Vec<(Arc<str>, Value)> = Vec::new();
    for _ in 0..pairs {
        let n = r.u2().map_err(|e| Exc::Internal(e.0))?;
        let element = cf.utf8(n).map_err(|e| Exc::Internal(e.0))?.text.clone();
        let ret = vm.class(ac).method_order.iter().map(|&m| vm.method(m)).find(|m| m.name == element && m.desc.starts_with("()")).map(|m| m.desc[2..].to_string()).unwrap_or_else(|| "Ljava/lang/Object;".into());
        let v = element_value(vm, cf, r, &ret)?;
        values.push((element, v));
    }
    let index = proxy_class(vm, &[ac], Some(ac))?;
    let class = vm.jdk.proxies[index].class;
    let obj = vm.new_instance(class)?;
    let mut items = Vec::new();
    for (n, v) in values {
        items.push(Value::Ref(vm.intern(&n)?));
        items.push(v);
    }
    if let Some(o) = vm.obj_mut(obj) {
        o.data = Data::Jdk(Box::new(State::Generic(items)));
    }
    Ok(Some((ac, obj)))
}

/// An element value, as the element's return type `ret` holds it.
fn element_value(vm: &mut Vm, cf: &ClassFile, r: &mut Reader, ret: &str) -> Res<Value> {
    let tag = r.u1().map_err(|e| Exc::Internal(e.0))?;
    Ok(match tag {
        b'B' | b'C' | b'I' | b'S' | b'Z' | b'J' | b'F' | b'D' => {
            let i = r.u2().map_err(|e| Exc::Internal(e.0))?;
            match cf.get(i).map_err(|e| Exc::Internal(e.0))? {
                Const::Int(v) => Value::Int(*v),
                Const::Long(v) => Value::Long(*v),
                Const::Float(v) => Value::Float(*v),
                Const::Double(v) => Value::Double(*v),
                _ => Value::Int(0),
            }
        }
        b's' => {
            let i = r.u2().map_err(|e| Exc::Internal(e.0))?;
            let units = cf.utf8(i).map_err(|e| Exc::Internal(e.0))?.units.clone();
            Value::Ref(vm.intern_units(&units)?)
        }
        b'e' => {
            let t = r.u2().map_err(|e| Exc::Internal(e.0))?;
            let n = r.u2().map_err(|e| Exc::Internal(e.0))?;
            let tdesc = cf.utf8(t).map_err(|e| Exc::Internal(e.0))?.text.to_string();
            let name = cf.utf8(n).map_err(|e| Exc::Internal(e.0))?.text.to_string();
            vm.get_static(tdesc.trim_start_matches('L').trim_end_matches(';'), &name, &tdesc)?
        }
        b'c' => {
            let i = r.u2().map_err(|e| Exc::Internal(e.0))?;
            let d = cf.utf8(i).map_err(|e| Exc::Internal(e.0))?.text.to_string();
            let c = vm.class_of_desc(&d)?;
            Value::Ref(vm.mirror(c)?)
        }
        b'@' => match annotation_value(vm, cf, r)? {
            Some((_, o)) => Value::Ref(o),
            None => Value::NULL,
        },
        b'[' => {
            let n = r.u2().map_err(|e| Exc::Internal(e.0))?;
            let elem = ret.strip_prefix('[').unwrap_or("Ljava/lang/Object;").to_string();
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(element_value(vm, cf, r, &elem)?);
            }
            let arr = vm.new_array_of(&elem, items.len() as i32)?;
            if let Some(a) = vm.array_mut(arr) {
                for (i, v) in items.into_iter().enumerate() {
                    a.set(i, v);
                }
            }
            Value::Ref(arr)
        }
        _ => return Err(Exc::Internal(format!("annotation element tag {tag}"))),
    })
}

fn annotation_of(vm: &mut Vm, e: ElementOf, wanted: ClassId) -> Res<ObjId> {
    Ok(annotations(vm, e)?.into_iter().find(|(c, _)| *c == wanted).map_or(0, |(_, o)| o))
}

fn annotations_array(vm: &mut Vm, e: ElementOf) -> Res<Value> {
    let objs: Vec<ObjId> = annotations(vm, e)?.into_iter().map(|(_, o)| o).collect();
    ret_ref(vm.new_ref_array("Ljava/lang/annotation/Annotation;", &objs)?)
}

/// The element values of an annotation object, the annotation's defaults filled in.
fn annotation_elements(vm: &mut Vm, obj: ObjId, ann: ClassId) -> Res<Vec<(Arc<str>, String, Value)>> {
    let given: Vec<Value> = match state(vm, obj) {
        Some(State::Generic(v)) => v.clone(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for m in vm.class(ann).method_order.clone() {
        let method = vm.method(m).clone();
        if method.is_static() || !method.desc.starts_with("()") {
            continue;
        }
        let ret = method.desc[2..].to_string();
        let mut value = None;
        for pair in given.chunks(2) {
            if vm.string(pair[0].r()).as_deref() == Some(&*method.name) {
                value = Some(pair[1]);
            }
        }
        let value = match value {
            Some(v) => v,
            None => {
                let Some(bytes) = method.info.as_ref().and_then(|i| i.annotation_default.clone()) else { continue };
                let Some(cf) = vm.class(ann).class_file().cloned() else { continue };
                let mut r = Reader::new(&bytes);
                element_value(vm, &cf, &mut r, &ret)?
            }
        };
        out.push((method.name.clone(), ret, value));
    }
    Ok(out)
}

fn annotation_call(vm: &mut Vm, obj: ObjId, ann: ClassId, name: &str, desc: &str, args: &[Value]) -> Res<Value> {
    match (name, desc) {
        ("annotationType", _) => ret_ref(vm.mirror(ann)?),
        ("toString", "()Ljava/lang/String;") => {
            let elements = annotation_elements(vm, obj, ann)?;
            let mut parts = Vec::new();
            for (n, ret, v) in &elements {
                parts.push((n.to_string(), element_text(vm, *v, ret)?));
            }
            let body = if parts.len() == 1 && parts[0].0 == "value" {
                parts[0].1.clone()
            } else {
                parts.iter().map(|(n, t)| format!("{n}={t}")).collect::<Vec<_>>().join(", ")
            };
            let cname = canonical_name(vm, ann).unwrap_or_else(|| vm.class(ann).java_name());
            ret_str(vm, &format!("@{cname}({body})"))
        }
        ("hashCode", "()I") => {
            let elements = annotation_elements(vm, obj, ann)?;
            let mut h: i32 = 0;
            for (n, ret, v) in elements {
                let nu: Vec<u16> = n.encode_utf16().collect();
                let vh = if ret.starts_with('[') {
                    let wrapped = vm.invoke_static("java/util/Arrays", "hashCode", &format!("({})I", if ret.len() == 2 { ret.clone() } else { "[Ljava/lang/Object;".into() }), &[v])?;
                    wrapped.i()
                } else {
                    super::lang::hash_of(vm, v, &ret)?
                };
                h = h.wrapping_add(127i32.wrapping_mul(super::string::java_hash(&nu)) ^ vh);
            }
            Ok(Value::Int(h))
        }
        ("equals", "(Ljava/lang/Object;)Z") => {
            let other = args.get(1).map_or(0, |v| v.r());
            if other == obj {
                return ret_bool(true);
            }
            if other == 0 || !vm.instance_of(other, ann) {
                return ret_bool(false);
            }
            let mine = annotation_elements(vm, obj, ann)?;
            for (n, ret, v) in mine {
                let theirs = vm.invoke_virtual(other, &n, &format!("(){ret}"), &[])?;
                let same = if ret.starts_with('[') {
                    vm.invoke_static("java/util/Arrays", "equals", &format!("({0}{0})Z", if ret.len() == 2 { ret.clone() } else { "[Ljava/lang/Object;".into() }), &[v, theirs])?.z()
                } else if ret.len() == 1 {
                    v == theirs
                } else {
                    super::lang::objects_equals(vm, v.r(), theirs.r())?
                };
                if !same {
                    return ret_bool(false);
                }
            }
            ret_bool(true)
        }
        _ => {
            let elements = annotation_elements(vm, obj, ann)?;
            match elements.into_iter().find(|(n, _, _)| **n == *name) {
                Some((_, ret, v)) => {
                    if ret.starts_with('[') && v.r() != 0 {
                        // Each call returns a copy, as Java's does.
                        let items = vm.array_values(v.r());
                        let elem = ret[1..].to_string();
                        let arr = vm.new_array_of(&elem, items.len() as i32)?;
                        if let Some(a) = vm.array_mut(arr) {
                            for (i, x) in items.into_iter().enumerate() {
                                a.set(i, x);
                            }
                        }
                        return ret_ref(arr);
                    }
                    Ok(v)
                }
                None => Err(vm.throw_new("java/lang/annotation/IncompleteAnnotationException", Some(name))),
            }
        }
    }
}

/// An element value as an annotation's `toString` writes it.
fn element_text(vm: &mut Vm, v: Value, ret: &str) -> Res<String> {
    Ok(match ret.as_bytes()[0] {
        b'[' => {
            let items = vm.array_values(v.r());
            let mut parts = Vec::new();
            for x in items {
                parts.push(element_text(vm, x, &ret[1..])?);
            }
            format!("{{{}}}", parts.join(", "))
        }
        b'J' => format!("{}L", v.j()),
        b'F' => {
            let f = v.f();
            if f.is_nan() { "0.0f/0.0f".into() } else if f.is_infinite() { if f > 0.0 { "1.0f/0.0f".into() } else { "-1.0f/0.0f".into() } } else { format!("{}f", crate::vm::java_float_to_string(f)) }
        }
        b'D' => {
            let d = v.d();
            if d.is_nan() { "0.0/0.0".into() } else if d.is_infinite() { if d > 0.0 { "1.0/0.0".into() } else { "-1.0/0.0".into() } } else { crate::vm::java_double_to_string(d) }
        }
        b'C' => format!("'{}'", quote_char(v.i() as u16, '\'')),
        b'B' => format!("(byte)0x{:02x}", v.i() as u8),
        b'S' | b'I' => v.i().to_string(),
        b'Z' => (v.i() != 0).to_string(),
        _ => {
            let r = v.r();
            if let Some(units) = vm.string_units(r) {
                let units = units.to_vec();
                format!("\"{}\"", units.iter().map(|&u| quote_char(u, '"')).collect::<String>())
            } else if let Some(c) = vm.mirror_class(r) {
                format!("{}.class", canonical_name(vm, c).unwrap_or_else(|| vm.class(c).java_name()))
            } else if let Some(n) = super::lang::enum_name(vm, r) {
                n
            } else {
                String::from_utf16_lossy(&vm.stringify(v, "Ljava/lang/Object;")?)
            }
        }
    })
}

fn quote_char(u: u16, quote: char) -> String {
    match u {
        0x08 => "\\b".into(),
        0x09 => "\\t".into(),
        0x0A => "\\n".into(),
        0x0C => "\\f".into(),
        0x0D => "\\r".into(),
        0x5C => "\\\\".into(),
        x if x == quote as u16 => format!("\\{quote}"),
        x if (0x20..0x7F).contains(&x) => (x as u8 as char).to_string(),
        x => format!("\\u{x:04x}"),
    }
}

// ------------------------------------------------------------------ classes

static CLASS_METHODS: &[NativeMethod] = natives![
    "forName" "(Ljava/lang/String;)Ljava/lang/Class;" S => Some(|vm, a| class_op(vm, a, "forName")),
    "forName" "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;" S => Some(|vm, a| class_op(vm, a, "forName3")),
    "forPrimitiveName" "(Ljava/lang/String;)Ljava/lang/Class;" S => Some(|vm, a| class_op(vm, a, "forPrimitiveName")),
    "getName" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "getName")),
    "getSimpleName" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "getSimpleName")),
    "getTypeName" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "getTypeName")),
    "getCanonicalName" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "getCanonicalName")),
    "getPackageName" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "getPackageName")),
    "getPackage" "()Ljava/lang/Package;" M => Some(|vm, a| class_op(vm, a, "getPackage")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "toString")),
    "descriptorString" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "descriptorString")),
    "getSuperclass" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getSuperclass")),
    "getInterfaces" "()[Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getInterfaces")),
    "isInterface" "()Z" M => Some(|vm, a| class_op(vm, a, "isInterface")),
    "isArray" "()Z" M => Some(|vm, a| class_op(vm, a, "isArray")),
    "isPrimitive" "()Z" M => Some(|vm, a| class_op(vm, a, "isPrimitive")),
    "isEnum" "()Z" M => Some(|vm, a| class_op(vm, a, "isEnum")),
    "isRecord" "()Z" M => Some(|vm, a| class_op(vm, a, "isRecord")),
    "isAnnotation" "()Z" M => Some(|vm, a| class_op(vm, a, "isAnnotation")),
    "isSynthetic" "()Z" M => Some(|vm, a| class_op(vm, a, "isSynthetic")),
    "isHidden" "()Z" M => Some(|vm, a| class_op(vm, a, "isHidden")),
    "isAnonymousClass" "()Z" M => Some(|vm, a| class_op(vm, a, "isAnonymousClass")),
    "isLocalClass" "()Z" M => Some(|vm, a| class_op(vm, a, "isLocalClass")),
    "isMemberClass" "()Z" M => Some(|vm, a| class_op(vm, a, "isMemberClass")),
    "isSealed" "()Z" M => Some(|vm, a| class_op(vm, a, "isSealed")),
    "getPermittedSubclasses" "()[Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getPermittedSubclasses")),
    "isInstance" "(Ljava/lang/Object;)Z" M => Some(|vm, a| class_op(vm, a, "isInstance")),
    "isAssignableFrom" "(Ljava/lang/Class;)Z" M => Some(|vm, a| class_op(vm, a, "isAssignableFrom")),
    "cast" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| class_op(vm, a, "cast")),
    "getComponentType" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getComponentType")),
    "componentType" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getComponentType")),
    "arrayType" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "arrayType")),
    "getModifiers" "()I" M => Some(|vm, a| class_op(vm, a, "getModifiers")),
    "getClassLoader" "()Ljava/lang/ClassLoader;" M => Some(|vm, a| class_op(vm, a, "getClassLoader")),
    "desiredAssertionStatus" "()Z" M => Some(|vm, a| class_op(vm, a, "desiredAssertionStatus")),
    "getEnumConstants" "()[Ljava/lang/Object;" M => Some(|vm, a| class_op(vm, a, "getEnumConstants")),
    "newInstance" "()Ljava/lang/Object;" M => Some(|vm, a| class_op(vm, a, "newInstance")),
    "getDeclaredMethods" "()[Ljava/lang/reflect/Method;" M => Some(|vm, a| class_op(vm, a, "getDeclaredMethods")),
    "getMethods" "()[Ljava/lang/reflect/Method;" M => Some(|vm, a| class_op(vm, a, "getMethods")),
    "getDeclaredConstructors" "()[Ljava/lang/reflect/Constructor;" M => Some(|vm, a| class_op(vm, a, "getDeclaredConstructors")),
    "getConstructors" "()[Ljava/lang/reflect/Constructor;" M => Some(|vm, a| class_op(vm, a, "getConstructors")),
    "getDeclaredMethod" "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;" M => Some(|vm, a| class_op(vm, a, "getDeclaredMethod")),
    "getMethod" "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;" M => Some(|vm, a| class_op(vm, a, "getMethod")),
    "getDeclaredConstructor" "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;" M => Some(|vm, a| class_op(vm, a, "getDeclaredConstructor")),
    "getConstructor" "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;" M => Some(|vm, a| class_op(vm, a, "getConstructor")),
    "getDeclaredFields" "()[Ljava/lang/reflect/Field;" M => Some(|vm, a| class_op(vm, a, "getDeclaredFields")),
    "getFields" "()[Ljava/lang/reflect/Field;" M => Some(|vm, a| class_op(vm, a, "getFields")),
    "getDeclaredField" "(Ljava/lang/String;)Ljava/lang/reflect/Field;" M => Some(|vm, a| class_op(vm, a, "getDeclaredField")),
    "getField" "(Ljava/lang/String;)Ljava/lang/reflect/Field;" M => Some(|vm, a| class_op(vm, a, "getField")),
    "getRecordComponents" "()[Ljava/lang/reflect/RecordComponent;" M => Some(|vm, a| class_op(vm, a, "getRecordComponents")),
    "getEnclosingClass" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getEnclosingClass")),
    "getDeclaringClass" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getDeclaringClass")),
    "getNestHost" "()Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getNestHost")),
    "isNestmateOf" "(Ljava/lang/Class;)Z" M => Some(|vm, a| class_op(vm, a, "isNestmateOf")),
    "getDeclaredClasses" "()[Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getDeclaredClasses")),
    "getClasses" "()[Ljava/lang/Class;" M => Some(|vm, a| class_op(vm, a, "getClasses")),
    "getResourceAsStream" "(Ljava/lang/String;)Ljava/io/InputStream;" M => Some(|vm, a| class_op(vm, a, "getResourceAsStream")),
    "getResource" "(Ljava/lang/String;)Ljava/net/URL;" M => Some(|vm, a| class_op(vm, a, "getResource")),
    "getAnnotation" "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;" M => Some(|vm, a| class_op(vm, a, "getAnnotation")),
    "getDeclaredAnnotation" "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;" M => Some(|vm, a| class_op(vm, a, "getDeclaredAnnotation")),
    "isAnnotationPresent" "(Ljava/lang/Class;)Z" M => Some(|vm, a| class_op(vm, a, "isAnnotationPresent")),
    "getAnnotations" "()[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| class_op(vm, a, "getAnnotations")),
    "getDeclaredAnnotations" "()[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| class_op(vm, a, "getDeclaredAnnotations")),
    "getAnnotationsByType" "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| class_op(vm, a, "getAnnotationsByType")),
    "getGenericSuperclass" "()Ljava/lang/reflect/Type;" M => Some(|vm, a| class_op(vm, a, "getGenericSuperclass")),
    "getGenericInterfaces" "()[Ljava/lang/reflect/Type;" M => Some(|vm, a| class_op(vm, a, "getGenericInterfaces")),
    "getTypeParameters" "()[Ljava/lang/reflect/TypeVariable;" M => Some(|vm, a| class_op(vm, a, "getTypeParameters")),
    "getEnclosingMethod" "()Ljava/lang/reflect/Method;" M => Some(|vm, a| class_op(vm, a, "getEnclosingMethod")),
    "getEnclosingConstructor" "()Ljava/lang/reflect/Constructor;" M => Some(|vm, a| class_op(vm, a, "getEnclosingConstructor")),
    "getModule" "()Ljava/lang/Module;" M => Some(|vm, a| class_op(vm, a, "getModule")),
    "hashCode" "()I" M => Some(|vm, a| class_op(vm, a, "hashCode")),
];

static EXECUTABLE_METHODS: &[NativeMethod] = natives![
    "getName" "()Ljava/lang/String;" M => Some(|vm, a| executable_op(vm, a, "getName")),
    "getDeclaringClass" "()Ljava/lang/Class;" M => Some(|vm, a| executable_op(vm, a, "getDeclaringClass")),
    "getModifiers" "()I" M => Some(|vm, a| executable_op(vm, a, "getModifiers")),
    "getParameterTypes" "()[Ljava/lang/Class;" M => Some(|vm, a| executable_op(vm, a, "getParameterTypes")),
    "getGenericParameterTypes" "()[Ljava/lang/reflect/Type;" M => Some(|vm, a| executable_op(vm, a, "getParameterTypes")),
    "getParameterCount" "()I" M => Some(|vm, a| executable_op(vm, a, "getParameterCount")),
    "getReturnType" "()Ljava/lang/Class;" M => Some(|vm, a| executable_op(vm, a, "getReturnType")),
    "getGenericReturnType" "()Ljava/lang/reflect/Type;" M => Some(|vm, a| executable_op(vm, a, "getGenericReturnType")),
    "getExceptionTypes" "()[Ljava/lang/Class;" M => Some(|vm, a| executable_op(vm, a, "getExceptionTypes")),
    "getParameters" "()[Ljava/lang/reflect/Parameter;" M => Some(|vm, a| executable_op(vm, a, "getParameters")),
    "isVarArgs" "()Z" M => Some(|vm, a| executable_op(vm, a, "isVarArgs")),
    "isSynthetic" "()Z" M => Some(|vm, a| executable_op(vm, a, "isSynthetic")),
    "isBridge" "()Z" M => Some(|vm, a| executable_op(vm, a, "isBridge")),
    "isDefault" "()Z" M => Some(|vm, a| executable_op(vm, a, "isDefault")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| executable_op(vm, a, "toString")),
    "toGenericString" "()Ljava/lang/String;" M => Some(|vm, a| executable_op(vm, a, "toGenericString")),
    "invoke" "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| executable_op(vm, a, "invoke")),
    "newInstance" "([Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| executable_op(vm, a, "newInstance")),
    "getAnnotation" "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;" M => Some(|vm, a| executable_op(vm, a, "getAnnotation")),
    "getDeclaredAnnotation" "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;" M => Some(|vm, a| executable_op(vm, a, "getDeclaredAnnotation")),
    "isAnnotationPresent" "(Ljava/lang/Class;)Z" M => Some(|vm, a| executable_op(vm, a, "isAnnotationPresent")),
    "getAnnotations" "()[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| executable_op(vm, a, "getAnnotations")),
    "getDeclaredAnnotations" "()[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| executable_op(vm, a, "getDeclaredAnnotations")),
    "getDefaultValue" "()Ljava/lang/Object;" M => Some(|vm, a| executable_op(vm, a, "getDefaultValue")),
    "setAccessible" "(Z)V" M => Some(|vm, a| common_op(vm, a, "setAccessible")),
    "trySetAccessible" "()Z" M => Some(|vm, a| common_op(vm, a, "trySetAccessible")),
    "isAccessible" "()Z" M => Some(|vm, a| common_op(vm, a, "isAccessible")),
    "canAccess" "(Ljava/lang/Object;)Z" M => Some(|vm, a| common_op(vm, a, "canAccess")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| common_op(vm, a, "equals")),
    "hashCode" "()I" M => Some(|vm, a| common_op(vm, a, "hashCode")),
];

macro_rules! field_access {
    ($($name:literal $desc:literal),* $(,)?) => {
        natives![
            "getName" "()Ljava/lang/String;" M => Some(|vm, a| field_op(vm, a, "getName")),
            "getDeclaringClass" "()Ljava/lang/Class;" M => Some(|vm, a| field_op(vm, a, "getDeclaringClass")),
            "getModifiers" "()I" M => Some(|vm, a| field_op(vm, a, "getModifiers")),
            "getType" "()Ljava/lang/Class;" M => Some(|vm, a| field_op(vm, a, "getType")),
            "getGenericType" "()Ljava/lang/reflect/Type;" M => Some(|vm, a| field_op(vm, a, "getGenericType")),
            "isEnumConstant" "()Z" M => Some(|vm, a| field_op(vm, a, "isEnumConstant")),
            "isSynthetic" "()Z" M => Some(|vm, a| field_op(vm, a, "isSynthetic")),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| field_op(vm, a, "toString")),
            "toGenericString" "()Ljava/lang/String;" M => Some(|vm, a| field_op(vm, a, "toGenericString")),
            "getAnnotation" "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;" M => Some(|vm, a| field_op(vm, a, "getAnnotation")),
            "getDeclaredAnnotation" "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;" M => Some(|vm, a| field_op(vm, a, "getDeclaredAnnotation")),
            "isAnnotationPresent" "(Ljava/lang/Class;)Z" M => Some(|vm, a| field_op(vm, a, "isAnnotationPresent")),
            "getAnnotations" "()[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| field_op(vm, a, "getAnnotations")),
            "getDeclaredAnnotations" "()[Ljava/lang/annotation/Annotation;" M => Some(|vm, a| field_op(vm, a, "getDeclaredAnnotations")),
            "setAccessible" "(Z)V" M => Some(|vm, a| common_op(vm, a, "setAccessible")),
            "trySetAccessible" "()Z" M => Some(|vm, a| common_op(vm, a, "trySetAccessible")),
            "isAccessible" "()Z" M => Some(|vm, a| common_op(vm, a, "isAccessible")),
            "canAccess" "(Ljava/lang/Object;)Z" M => Some(|vm, a| common_op(vm, a, "canAccess")),
            "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| common_op(vm, a, "equals")),
            "hashCode" "()I" M => Some(|vm, a| common_op(vm, a, "hashCode")),
            $( $name $desc M => Some(|vm, a| field_op(vm, a, $name)), )*
        ]
    };
}

static FIELD_METHODS: &[NativeMethod] = field_access![
    "get" "(Ljava/lang/Object;)Ljava/lang/Object;",
    "getInt" "(Ljava/lang/Object;)I",
    "getLong" "(Ljava/lang/Object;)J",
    "getDouble" "(Ljava/lang/Object;)D",
    "getFloat" "(Ljava/lang/Object;)F",
    "getBoolean" "(Ljava/lang/Object;)Z",
    "getByte" "(Ljava/lang/Object;)B",
    "getChar" "(Ljava/lang/Object;)C",
    "getShort" "(Ljava/lang/Object;)S",
    "set" "(Ljava/lang/Object;Ljava/lang/Object;)V",
    "setInt" "(Ljava/lang/Object;I)V",
    "setLong" "(Ljava/lang/Object;J)V",
    "setDouble" "(Ljava/lang/Object;D)V",
    "setFloat" "(Ljava/lang/Object;F)V",
    "setBoolean" "(Ljava/lang/Object;Z)V",
    "setByte" "(Ljava/lang/Object;B)V",
    "setChar" "(Ljava/lang/Object;C)V",
    "setShort" "(Ljava/lang/Object;S)V",
];

static CLASSES: &[NativeClass] = &[
    NativeClass {
        name: "java/lang/Class",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable", "java/lang/reflect/GenericDeclaration", "java/lang/reflect/Type", "java/lang/reflect/AnnotatedElement"],
        access: PUBLIC | FINAL,
        methods: CLASS_METHODS,
        statics: &[],
        clinit: None,
        data: None,
    },
    class("java/lang/reflect/Type", None, &[], INTERFACE, natives!["getTypeName" "()Ljava/lang/String;" M => Some(|vm, a| class_op(vm, a, "getTypeName"))]),
    class("java/lang/reflect/AnnotatedElement", None, &[], INTERFACE, &[]),
    class("java/lang/reflect/GenericDeclaration", None, &["java/lang/reflect/AnnotatedElement"], INTERFACE, &[]),
    class("java/lang/reflect/Member", None, &[], INTERFACE, natives!["getName" "()Ljava/lang/String;" A => None]),
    class("java/lang/reflect/TypeVariable", None, &["java/lang/reflect/Type"], INTERFACE, &[]),
    class("java/lang/reflect/InvocationHandler", None, &[], INTERFACE, natives!["invoke" "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;" A => None]),
    class("java/lang/annotation/Annotation", None, &[], INTERFACE, natives!["annotationType" "()Ljava/lang/Class;" A => None]),
    class("java/lang/annotation/Inherited", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/annotation/Retention", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/annotation/Target", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/annotation/Documented", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/annotation/Repeatable", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/FunctionalInterface", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/Deprecated", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/SafeVarargs", None, &["java/lang/annotation/Annotation"], INTERFACE | 0x2000, &[]),
    class("java/lang/annotation/IncompleteAnnotationException", Some("java/lang/RuntimeException"), &[], PUBLIC, super::lang::THROWABLE_CTORS),
    class("java/lang/reflect/AccessibleObject", Some("java/lang/Object"), &["java/lang/reflect/AnnotatedElement"], PUBLIC, natives![]),
    class("java/lang/reflect/Executable", Some("java/lang/reflect/AccessibleObject"), &["java/lang/reflect/Member", "java/lang/reflect/GenericDeclaration"], PUBLIC | 0x0400, natives![]),
    class("java/lang/reflect/Method", Some("java/lang/reflect/Executable"), &[], PUBLIC | FINAL, EXECUTABLE_METHODS),
    class("java/lang/reflect/Constructor", Some("java/lang/reflect/Executable"), &[], PUBLIC | FINAL, EXECUTABLE_METHODS),
    class("java/lang/reflect/Field", Some("java/lang/reflect/AccessibleObject"), &["java/lang/reflect/Member"], PUBLIC | FINAL, FIELD_METHODS),
    class("java/lang/reflect/RecordComponent", Some("java/lang/Object"), &["java/lang/reflect/AnnotatedElement"], PUBLIC | FINAL, natives![
        "getName" "()Ljava/lang/String;" M => Some(|vm, a| record_component_op(vm, a, "getName")),
        "getType" "()Ljava/lang/Class;" M => Some(|vm, a| record_component_op(vm, a, "getType")),
        "getGenericType" "()Ljava/lang/reflect/Type;" M => Some(|vm, a| record_component_op(vm, a, "getGenericType")),
        "getAccessor" "()Ljava/lang/reflect/Method;" M => Some(|vm, a| record_component_op(vm, a, "getAccessor")),
        "getDeclaringRecord" "()Ljava/lang/Class;" M => Some(|vm, a| record_component_op(vm, a, "getDeclaringRecord")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| record_component_op(vm, a, "toString")),
    ]),
    class("java/lang/reflect/Parameter", Some("java/lang/Object"), &["java/lang/reflect/AnnotatedElement"], PUBLIC | FINAL, natives![
        "getName" "()Ljava/lang/String;" M => Some(|vm, a| parameter_op(vm, a, "getName")),
        "isNamePresent" "()Z" M => Some(|vm, a| parameter_op(vm, a, "isNamePresent")),
        "getType" "()Ljava/lang/Class;" M => Some(|vm, a| parameter_op(vm, a, "getType")),
        "getParameterizedType" "()Ljava/lang/reflect/Type;" M => Some(|vm, a| parameter_op(vm, a, "getParameterizedType")),
        "getModifiers" "()I" M => Some(|vm, a| parameter_op(vm, a, "getModifiers")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| parameter_op(vm, a, "toString")),
    ]),
    class("java/lang/reflect/Modifier", Some("java/lang/Object"), &[], PUBLIC, natives![
        "toString" "(I)Ljava/lang/String;" S => Some(|vm, a| { let s = modifier_text(arg(a, 0).i()); ret_str(vm, &s) }),
        "isPublic" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0001 != 0)),
        "isPrivate" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0002 != 0)),
        "isProtected" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0004 != 0)),
        "isStatic" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0008 != 0)),
        "isFinal" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0010 != 0)),
        "isSynchronized" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0020 != 0)),
        "isVolatile" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0040 != 0)),
        "isTransient" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0080 != 0)),
        "isNative" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0100 != 0)),
        "isInterface" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0200 != 0)),
        "isAbstract" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0400 != 0)),
        "isStrict" "(I)Z" S => Some(|_, a| ret_bool(arg(a, 0).i() & 0x0800 != 0)),
    ]),
    class("java/lang/reflect/Array", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![
        "newInstance" "(Ljava/lang/Class;I)Ljava/lang/Object;" S => Some(|vm, a| array_op(vm, a, "newInstance")),
        "newInstance" "(Ljava/lang/Class;[I)Ljava/lang/Object;" S => Some(|vm, a| array_op(vm, a, "newInstanceDims")),
        "getLength" "(Ljava/lang/Object;)I" S => Some(|vm, a| array_op(vm, a, "getLength")),
        "get" "(Ljava/lang/Object;I)Ljava/lang/Object;" S => Some(|vm, a| array_op(vm, a, "get")),
        "getInt" "(Ljava/lang/Object;I)I" S => Some(|vm, a| array_op(vm, a, "getInt")),
        "getLong" "(Ljava/lang/Object;I)J" S => Some(|vm, a| array_op(vm, a, "getLong")),
        "getDouble" "(Ljava/lang/Object;I)D" S => Some(|vm, a| array_op(vm, a, "getDouble")),
        "getFloat" "(Ljava/lang/Object;I)F" S => Some(|vm, a| array_op(vm, a, "getFloat")),
        "getBoolean" "(Ljava/lang/Object;I)Z" S => Some(|vm, a| array_op(vm, a, "getBoolean")),
        "getChar" "(Ljava/lang/Object;I)C" S => Some(|vm, a| array_op(vm, a, "getChar")),
        "getByte" "(Ljava/lang/Object;I)B" S => Some(|vm, a| array_op(vm, a, "getByte")),
        "getShort" "(Ljava/lang/Object;I)S" S => Some(|vm, a| array_op(vm, a, "getShort")),
        "set" "(Ljava/lang/Object;ILjava/lang/Object;)V" S => Some(|vm, a| array_op(vm, a, "set")),
        "setInt" "(Ljava/lang/Object;II)V" S => Some(|vm, a| array_op(vm, a, "setInt")),
        "setLong" "(Ljava/lang/Object;IJ)V" S => Some(|vm, a| array_op(vm, a, "setLong")),
        "setDouble" "(Ljava/lang/Object;ID)V" S => Some(|vm, a| array_op(vm, a, "setDouble")),
        "setBoolean" "(Ljava/lang/Object;IZ)V" S => Some(|vm, a| array_op(vm, a, "setBoolean")),
        "setChar" "(Ljava/lang/Object;IC)V" S => Some(|vm, a| array_op(vm, a, "setChar")),
    ]),
    class("java/lang/reflect/Proxy", Some("java/lang/Object"), &["java/io/Serializable"], PUBLIC, natives![
        "newProxyInstance" "(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)Ljava/lang/Object;" S => Some(proxy_new),
        "isProxyClass" "(Ljava/lang/Class;)Z" S => Some(|vm, a| proxy_static(vm, a, "isProxyClass")),
        "getInvocationHandler" "(Ljava/lang/Object;)Ljava/lang/reflect/InvocationHandler;" S => Some(|vm, a| proxy_static(vm, a, "getInvocationHandler")),
    ]),
    class("java/lang/ClassLoader", Some("java/lang/Object"), &[], PUBLIC | 0x0400, natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "getSystemClassLoader" "()Ljava/lang/ClassLoader;" S => Some(|vm, a| loader_op(vm, a, "getSystemClassLoader")),
        "getPlatformClassLoader" "()Ljava/lang/ClassLoader;" S => Some(|vm, a| loader_op(vm, a, "getPlatformClassLoader")),
        "loadClass" "(Ljava/lang/String;)Ljava/lang/Class;" M => Some(|vm, a| loader_op(vm, a, "loadClass")),
        "getResourceAsStream" "(Ljava/lang/String;)Ljava/io/InputStream;" M => Some(|vm, a| loader_op(vm, a, "getResourceAsStream")),
        "getSystemResourceAsStream" "(Ljava/lang/String;)Ljava/io/InputStream;" S => Some(|vm, a| { let l = app_loader(vm)?; loader_op(vm, &[Value::Ref(l), arg(a, 0)], "getResourceAsStream") }),
        "getResource" "(Ljava/lang/String;)Ljava/net/URL;" M => Some(|vm, a| loader_op(vm, a, "getResource")),
        "getName" "()Ljava/lang/String;" M => Some(|vm, a| loader_op(vm, a, "getName")),
        "getParent" "()Ljava/lang/ClassLoader;" M => Some(|vm, a| loader_op(vm, a, "getParent")),
    ]),
    NativeClass {
        name: "jdk/internal/loader/ClassLoaders$AppClassLoader",
        super_name: Some("java/lang/ClassLoader"),
        interfaces: &[],
        access: FINAL,
        methods: natives!["toString" "()Ljava/lang/String;" M => Some(|vm, a| loader_op(vm, a, "toString"))],
        statics: &[("INSTANCE", "Ljava/lang/ClassLoader;")],
        clinit: None,
        data: None,
    },
    class("java/lang/Package", Some("java/lang/Object"), &["java/lang/reflect/AnnotatedElement"], PUBLIC, natives![
        "getName" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; Ok(match state(vm, t) { Some(State::Generic(v)) => v[0], _ => Value::NULL }) }),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let n = match state(vm, t) { Some(State::Generic(v)) => vm.string(v[0].r()).unwrap_or_default(), _ => String::new() }; ret_str(vm, &format!("package {n}")) }),
    ]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_text_uses_java_order() {
        // Modifier.toString(Modifier.PUBLIC | Modifier.STATIC | Modifier.FINAL | Modifier.SYNCHRONIZED)
        assert_eq!(modifier_text(0x0001 | 0x0008 | 0x0010 | 0x0020), "public static final synchronized");
        assert_eq!(modifier_text(0x0601), "public abstract interface");
    }
}

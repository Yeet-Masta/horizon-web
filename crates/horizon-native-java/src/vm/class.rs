//! Loading, linking and initializing classes (JVMS 5), and resolving and
//! selecting their members.
use super::classfile::{self, ClassFile, Code, MemberInfo};
use super::heap::Obj;
use super::interp::{Decoded, Vm};
use super::jdk::{self, NativeClass, NativeFn};
use super::{ClassId, Data, Exc, MethodId, NameDesc, ObjId, Res, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_PRIVATE: u16 = 0x0002;
pub const ACC_PROTECTED: u16 = 0x0004;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_FINAL: u16 = 0x0010;
pub const ACC_SYNCHRONIZED: u16 = 0x0020;
pub const ACC_VARARGS: u16 = 0x0080;
pub const ACC_NATIVE: u16 = 0x0100;
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;
pub const ACC_SYNTHETIC: u16 = 0x1000;
pub const ACC_ANNOTATION: u16 = 0x2000;
pub const ACC_ENUM: u16 = 0x4000;

#[derive(Debug, Clone)]
pub enum ClassKind {
    Bytecode(Arc<ClassFile>),
    Native(&'static NativeClass),
    Array { component: ClassId, elem: u8 },
    Primitive(u8),
    /// A class the VM made: a lambda's, a proxy's.
    Synthetic,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InitState {
    Linked,
    Initializing(usize),
    Initialized,
    Failed,
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: Arc<str>,
    pub desc: Arc<str>,
    pub access: u16,
    pub slot: usize,
    pub nd: NameDesc,
    pub signature: Option<Arc<str>>,
    pub annotations: Option<Arc<[u8]>>,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub id: ClassId,
    pub name: Arc<str>,
    pub access: u16,
    pub kind: ClassKind,
    pub super_class: Option<ClassId>,
    pub interfaces: Vec<ClassId>,
    /// Every instance field slot's starting value, inherited ones first.
    pub field_zeros: Box<[Value]>,
    /// The instance fields this class declares.
    pub fields: Vec<FieldInfo>,
    pub static_fields: Vec<FieldInfo>,
    pub statics: Vec<Value>,
    /// Declared methods by name and descriptor, and in declaration order.
    pub methods: HashMap<NameDesc, MethodId>,
    pub method_order: Vec<MethodId>,
    /// Virtual and interface selections already made on this class.
    pub selected: HashMap<NameDesc, MethodId>,
    pub state: InitState,
    pub mirror: ObjId,
    /// The JDK class this one is or extends, whose native state its instances carry.
    pub native_root: Option<&'static NativeClass>,
    /// The synthetic data of a VM-made class (a lambda's target, a proxy's handler).
    pub synthetic: Option<usize>,
}

impl Class {
    pub fn is_interface(&self) -> bool {
        self.access & ACC_INTERFACE != 0
    }
    pub fn is_array(&self) -> bool {
        matches!(self.kind, ClassKind::Array { .. })
    }
    pub fn class_file(&self) -> Option<&Arc<ClassFile>> {
        match &self.kind {
            ClassKind::Bytecode(cf) => Some(cf),
            _ => None,
        }
    }
    /// The Java binary name (`java.lang.String`, `[I`, `int`).
    pub fn java_name(&self) -> String {
        match self.kind {
            ClassKind::Primitive(p) => primitive_name(p).to_string(),
            // A hidden class: `pkg.Outer$$Lambda/0x...`.
            ClassKind::Synthetic if self.name.contains("$$Lambda/") => {
                let (outer, rest) = self.name.split_once("$$Lambda/").unwrap_or((&self.name, ""));
                format!("{}$$Lambda/{rest}", outer.replace('/', "."))
            }
            _ => self.name.replace('/', "."),
        }
    }
}

pub fn primitive_name(p: u8) -> &'static str {
    match p {
        b'Z' => "boolean",
        b'B' => "byte",
        b'C' => "char",
        b'S' => "short",
        b'I' => "int",
        b'J' => "long",
        b'F' => "float",
        b'D' => "double",
        _ => "void",
    }
}

#[derive(Debug, Clone)]
pub enum MethodBody {
    Bytecode(Arc<Code>),
    Native(NativeFn),
    Abstract,
    /// A lambda class's interface method: calls `vm.lambdas[index]`'s target.
    Lambda(usize),
    /// A proxy class's method: calls the proxy's `InvocationHandler`.
    Proxy,
}

#[derive(Debug, Clone)]
pub struct Method {
    pub id: MethodId,
    pub class: ClassId,
    pub name: Arc<str>,
    pub desc: Arc<str>,
    pub nd: NameDesc,
    pub access: u16,
    /// Slots the arguments take, `this` excluded.
    pub arg_slots: u16,
    /// The return descriptor's first byte (`V`, `I`, `J`, `L`, `[`...).
    pub ret: u8,
    pub body: MethodBody,
    pub decoded: Option<Box<Decoded>>,
    pub info: Option<Arc<MemberInfo>>,
}

impl Method {
    pub fn is_static(&self) -> bool {
        self.access & ACC_STATIC != 0
    }
    pub fn is_abstract(&self) -> bool {
        matches!(self.body, MethodBody::Abstract)
    }
    pub fn returns_wide(&self) -> bool {
        self.ret == b'J' || self.ret == b'D'
    }
}

impl Vm {
    /// The interned id of a `(name, descriptor)` pair.
    pub fn nd(&mut self, name: &str, desc: &str) -> NameDesc {
        let key = (Arc::<str>::from(name), Arc::<str>::from(desc));
        if let Some(&id) = self.nd_ids.get(&key) {
            return id;
        }
        let id = self.nd_names.len() as NameDesc;
        self.nd_names.push(key.clone());
        self.nd_ids.insert(key, id);
        id
    }

    pub fn nd_lookup(&self, name: &str, desc: &str) -> Option<NameDesc> {
        self.nd_ids
            .get(&(Arc::<str>::from(name), Arc::<str>::from(desc)))
            .copied()
    }

    pub fn class(&self, c: ClassId) -> &Class {
        &self.classes[c as usize]
    }

    pub fn class_name(&self, c: ClassId) -> &str {
        &self.classes[c as usize].name
    }

    pub fn method(&self, m: MethodId) -> &Method {
        &self.methods[m as usize]
    }

    pub fn find_loaded(&self, name: &str) -> Option<ClassId> {
        self.class_by_name.get(name).copied()
    }

    /// Loads `name` (an internal name or an array descriptor), throwing
    /// `NoClassDefFoundError` when the class path has no such class.
    pub fn load(&mut self, name: &str) -> Res<ClassId> {
        if let Some(c) = self.find_loaded(name) {
            return Ok(c);
        }
        match self.try_load(name)? {
            Some(c) => Ok(c),
            None => {
                if jdk::is_jdk_name(name) {
                    return Err(Exc::Unsupported(format!("JDK class {name}")));
                }
                Err(self.throw_new("java/lang/NoClassDefFoundError", Some(name)))
            }
        }
    }

    /// Loads `name`, answering `None` when no one has it (for `Class.forName`).
    pub fn try_load(&mut self, name: &str) -> Res<Option<ClassId>> {
        if let Some(c) = self.find_loaded(name) {
            return Ok(Some(c));
        }
        if name.starts_with('[') {
            return self.array_class(name).map(Some);
        }
        if let Some(def) = jdk::find(name) {
            return self.define_native(def).map(Some);
        }
        if self.loading.iter().any(|n| &**n == name) {
            return Err(self.throw_new("java/lang/ClassCircularityError", Some(name)));
        }
        let Some(bytes) = self.source.class_bytes(name) else {
            return Ok(None);
        };
        let cf = match ClassFile::parse(&bytes) {
            Ok(cf) => cf,
            Err(e) => {
                return Err(self.throw_new("java/lang/ClassFormatError", Some(&format!("{name}: {e}"))));
            }
        };
        if &*cf.name != name {
            let msg = format!("{name} (wrong name: {})", cf.name);
            return Err(self.throw_new("java/lang/NoClassDefFoundError", Some(&msg)));
        }
        if cf.name.starts_with("java/") {
            let msg = format!("Prohibited package name: {}", cf.name.rsplit_once('/').map_or("", |p| p.0).replace('/', "."));
            return Err(self.throw_new("java/lang/SecurityException", Some(&msg)));
        }
        self.loading.push(cf.name.clone());
        let result = self.define_bytecode(Arc::new(cf));
        self.loading.pop();
        result.map(Some)
    }

    fn new_class_record(&mut self, name: Arc<str>, access: u16, kind: ClassKind) -> ClassId {
        let id = self.classes.len() as ClassId;
        self.classes.push(Class {
            id,
            name: name.clone(),
            access,
            kind,
            super_class: None,
            interfaces: Vec::new(),
            field_zeros: Box::new([]),
            fields: Vec::new(),
            static_fields: Vec::new(),
            statics: Vec::new(),
            methods: HashMap::new(),
            method_order: Vec::new(),
            selected: HashMap::new(),
            state: InitState::Linked,
            mirror: 0,
            native_root: None,
            synthetic: None,
        });
        self.class_by_name.insert(name, id);
        id
    }

    /// Links a parsed class: superclass and interfaces first, then its
    /// layout, statics and methods.
    fn define_bytecode(&mut self, cf: Arc<ClassFile>) -> Res<ClassId> {
        let super_class = match &cf.super_name {
            Some(s) => {
                let s = self.load(s)?;
                if self.class(s).is_interface() {
                    return Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&format!("class {} has interface {} as super class", cf.name.replace('/', "."), self.class_name(s).replace('/', ".")))));
                }
                if self.class(s).access & ACC_FINAL != 0 {
                    return Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&format!("class {} cannot inherit from final class {}", cf.name.replace('/', "."), self.class_name(s).replace('/', ".")))));
                }
                Some(s)
            }
            None if &*cf.name == "java/lang/Object" => None,
            None => return Err(self.throw_new("java/lang/ClassFormatError", Some("no superclass"))),
        };
        let mut interfaces = Vec::with_capacity(cf.interfaces.len());
        for i in &cf.interfaces {
            let c = self.load(i)?;
            if !self.class(c).is_interface() {
                return Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&format!("class {} can not implement {}, because it is not an interface", cf.name.replace('/', "."), i.replace('/', ".")))));
            }
            interfaces.push(c);
        }
        if let Some(c) = self.find_loaded(&cf.name) {
            // Loading a superclass loaded this one (a cycle the guard let through).
            return Ok(c);
        }
        let id = self.new_class_record(cf.name.clone(), cf.access, ClassKind::Bytecode(cf.clone()));
        let mut zeros: Vec<Value> = super_class
            .map(|s| self.class(s).field_zeros.to_vec())
            .unwrap_or_default();
        let mut fields = Vec::new();
        let mut static_fields = Vec::new();
        let mut statics = Vec::new();
        for f in &cf.fields {
            let nd = self.nd(&f.name, &f.desc);
            let info = |slot| FieldInfo {
                name: f.name.clone(),
                desc: f.desc.clone(),
                access: f.access,
                slot,
                nd,
                signature: f.signature.clone(),
                annotations: f.annotations.clone(),
            };
            if f.access & ACC_STATIC != 0 {
                static_fields.push(info(statics.len()));
                statics.push(Value::zero(&f.desc));
            } else {
                fields.push(info(zeros.len()));
                zeros.push(Value::zero(&f.desc));
            }
        }
        let native_root = super_class.and_then(|s| self.class(s).native_root);
        {
            let c = &mut self.classes[id as usize];
            c.super_class = super_class;
            c.interfaces = interfaces;
            c.field_zeros = zeros.into_boxed_slice();
            c.fields = fields;
            c.static_fields = static_fields;
            c.statics = statics;
            c.native_root = native_root;
        }
        for m in &cf.methods {
            let body = match &m.code {
                Some(code) => MethodBody::Bytecode(code.clone()),
                None if m.access & ACC_NATIVE != 0 => {
                    match jdk::bytecode_class_native(&cf.name, &m.name, &m.desc) {
                        Some(f) => MethodBody::Native(f),
                        None => MethodBody::Abstract,
                    }
                }
                None => MethodBody::Abstract,
            };
            self.add_method(id, &m.name, &m.desc, m.access, body, Some(Arc::new(m.clone())))?;
        }
        Ok(id)
    }

    /// Adds one method to a class being defined.
    pub fn add_method(
        &mut self,
        class: ClassId,
        name: &str,
        desc: &str,
        access: u16,
        body: MethodBody,
        info: Option<Arc<MemberInfo>>,
    ) -> Res<MethodId> {
        let Some((params, ret)) = classfile::parse_method_desc(desc) else {
            return Err(self.throw_new("java/lang/ClassFormatError", Some(&format!("bad method descriptor {desc}"))));
        };
        let arg_slots = classfile::arg_slots(&params) as u16;
        let ret = ret.as_bytes()[0];
        let nd = self.nd(name, desc);
        let id = self.methods.len() as MethodId;
        self.methods.push(Method {
            id,
            class,
            name: name.into(),
            desc: desc.into(),
            nd,
            access,
            arg_slots,
            ret,
            body,
            decoded: None,
            info,
        });
        let c = &mut self.classes[class as usize];
        c.methods.insert(nd, id);
        c.method_order.push(id);
        Ok(id)
    }

    /// Defines a JDK class from its native description.
    fn define_native(&mut self, def: &'static NativeClass) -> Res<ClassId> {
        let super_class = match def.super_name {
            Some(s) => Some(self.load(s)?),
            None => None,
        };
        let mut interfaces = Vec::new();
        for i in def.interfaces {
            interfaces.push(self.load(i)?);
        }
        if let Some(c) = self.find_loaded(def.name) {
            return Ok(c);
        }
        let id = self.new_class_record(def.name.into(), def.access, ClassKind::Native(def));
        let zeros = super_class
            .map(|s| self.class(s).field_zeros.clone())
            .unwrap_or_default();
        let mut static_fields = Vec::new();
        let mut statics = Vec::new();
        for &(name, desc) in def.statics {
            let nd = self.nd(name, desc);
            static_fields.push(FieldInfo {
                name: name.into(),
                desc: desc.into(),
                access: ACC_PUBLIC | ACC_STATIC | ACC_FINAL,
                slot: statics.len(),
                nd,
                signature: None,
                annotations: None,
            });
            statics.push(Value::zero(desc));
        }
        {
            let c = &mut self.classes[id as usize];
            c.super_class = super_class;
            c.interfaces = interfaces;
            c.field_zeros = zeros;
            c.static_fields = static_fields;
            c.statics = statics;
            c.native_root = Some(def);
        }
        // A class whose method table a macro makes keeps the rest of its
        // methods in a second table, named `<class>$Extra`.
        let extra = jdk::find(&format!("{}$Extra", def.name)).map_or(&[][..], |e| e.methods);
        let extra2 = jdk::find(&format!("{}$Extra2", def.name)).map_or(&[][..], |e| e.methods);
        for m in def.methods.iter().chain(extra).chain(extra2) {
            let body = match m.f {
                Some(f) => MethodBody::Native(f),
                None => MethodBody::Abstract,
            };
            self.add_method(id, m.name, m.desc, m.access, body, None)?;
        }
        Ok(id)
    }

    /// The class of an array descriptor (`[I`, `[[Ljava/lang/String;`).
    pub fn array_class(&mut self, name: &str) -> Res<ClassId> {
        if let Some(c) = self.find_loaded(name) {
            return Ok(c);
        }
        let comp_desc = &name[1..];
        let (component, elem) = match comp_desc.as_bytes().first() {
            Some(b'L') if comp_desc.ends_with(';') => {
                (self.load(&comp_desc[1..comp_desc.len() - 1])?, b'L')
            }
            Some(b'[') => (self.array_class(comp_desc)?, b'L'),
            Some(&p) if b"ZBCSIJFD".contains(&p) && comp_desc.len() == 1 => (self.primitive_class(p), p),
            _ => return Err(self.throw_new("java/lang/NoClassDefFoundError", Some(name))),
        };
        let object = self.load("java/lang/Object")?;
        let cloneable = self.load("java/lang/Cloneable")?;
        let serializable = self.load("java/io/Serializable")?;
        let access = (self.class(component).access & ACC_PUBLIC) | ACC_FINAL | ACC_ABSTRACT;
        let id = self.new_class_record(name.into(), access, ClassKind::Array { component, elem });
        let c = &mut self.classes[id as usize];
        c.super_class = Some(object);
        c.interfaces = vec![cloneable, serializable];
        c.state = InitState::Initialized;
        Ok(id)
    }

    /// The array class whose elements have descriptor `elem_desc`.
    pub fn array_class_of_desc(&mut self, elem_desc: &str) -> Res<ClassId> {
        self.array_class(&format!("[{elem_desc}"))
    }

    /// The array class whose component is `component`.
    pub fn array_class_of(&mut self, component: ClassId) -> Res<ClassId> {
        let desc = self.descriptor_of(component);
        self.array_class_of_desc(&desc)
    }

    /// A class as a field descriptor (`I`, `Ljava/lang/String;`, `[J`).
    pub fn descriptor_of(&self, c: ClassId) -> String {
        let class = self.class(c);
        match class.kind {
            ClassKind::Primitive(p) => (p as char).to_string(),
            ClassKind::Array { .. } => class.name.to_string(),
            _ => format!("L{};", class.name),
        }
    }

    pub fn primitive_class(&mut self, p: u8) -> ClassId {
        let name = primitive_name(p);
        if let Some(c) = self.find_loaded(name) {
            return c;
        }
        let id = self.new_class_record(name.into(), ACC_PUBLIC | ACC_FINAL | ACC_ABSTRACT, ClassKind::Primitive(p));
        self.classes[id as usize].state = InitState::Initialized;
        id
    }

    /// The class a field descriptor or class name names, loading it.
    pub fn class_of_desc(&mut self, desc: &str) -> Res<ClassId> {
        match desc.as_bytes().first() {
            Some(b'L') if desc.ends_with(';') => self.load(&desc[1..desc.len() - 1]),
            Some(b'[') => self.array_class(desc),
            Some(&p) if desc.len() == 1 => Ok(self.primitive_class(p)),
            _ => self.load(desc),
        }
    }

    // ------------------------------------------------------------- subtyping

    /// Whether `sub` is `sup` or a subclass or subinterface of it, with arrays
    /// covariant as JVMS 6.5 `checkcast` says.
    pub fn is_subclass(&self, sub: ClassId, sup: ClassId) -> bool {
        if sub == sup {
            return true;
        }
        let s = self.class(sub);
        if let ClassKind::Array { component: sc, elem: se } = s.kind {
            return match self.class(sup).kind {
                ClassKind::Array { component: pc, elem: pe } => {
                    if se != b'L' || pe != b'L' {
                        sc == pc
                    } else {
                        self.is_subclass(sc, pc)
                    }
                }
                _ => {
                    let n = &*self.class(sup).name;
                    n == "java/lang/Object" || n == "java/lang/Cloneable" || n == "java/io/Serializable"
                }
            };
        }
        if let Some(parent) = s.super_class
            && self.is_subclass(parent, sup)
        {
            return true;
        }
        s.interfaces.iter().any(|&i| self.is_subclass(i, sup))
    }

    /// `instanceof`: whether a non-null reference's class is assignable to `c`.
    pub fn instance_of(&self, r: ObjId, c: ClassId) -> bool {
        self.class_of(r).is_some_and(|rc| self.is_subclass(rc, c))
    }

    // ------------------------------------------------------------- members

    /// Resolves a field reference (JVMS 5.4.3.2): the class, its
    /// superinterfaces, then its superclass.
    pub fn resolve_field(&mut self, class: ClassId, nd: NameDesc) -> Option<(ClassId, FieldInfo, bool)> {
        let c = self.class(class);
        if let Some(f) = c.fields.iter().find(|f| f.nd == nd) {
            return Some((class, f.clone(), false));
        }
        if let Some(f) = c.static_fields.iter().find(|f| f.nd == nd) {
            return Some((class, f.clone(), true));
        }
        let interfaces = c.interfaces.clone();
        let parent = c.super_class;
        for i in interfaces {
            if let Some(found) = self.resolve_field(i, nd) {
                return Some(found);
            }
        }
        parent.and_then(|p| self.resolve_field(p, nd))
    }

    /// Resolves a method reference (JVMS 5.4.3.3 and 5.4.3.4): the class and
    /// its superclasses, then the maximally-specific superinterface methods.
    pub fn resolve_method(&self, class: ClassId, nd: NameDesc) -> Option<MethodId> {
        let mut c = Some(class);
        while let Some(id) = c {
            let cls = self.class(id);
            if let Some(&m) = cls.methods.get(&nd) {
                return Some(m);
            }
            // Arrays answer Object's methods.
            c = cls.super_class;
        }
        let candidates = self.interface_methods(class, nd, false);
        candidates
            .iter()
            .copied()
            .find(|&m| !self.method(m).is_abstract())
            .or_else(|| candidates.first().copied())
    }

    /// The non-static, non-private methods `nd` that `class`'s superinterfaces
    /// declare, maximally specific first (no candidate's interface is a
    /// superinterface of another's).
    fn interface_methods(&self, class: ClassId, nd: NameDesc, defaults_only: bool) -> Vec<MethodId> {
        let mut seen = Vec::new();
        let mut stack = vec![class];
        let mut all = Vec::new();
        while let Some(c) = stack.pop() {
            let cls = self.class(c);
            for &i in &cls.interfaces {
                if !seen.contains(&i) {
                    seen.push(i);
                    stack.push(i);
                    let ic = self.class(i);
                    if let Some(&m) = ic.methods.get(&nd) {
                        let method = self.method(m);
                        if method.access & (ACC_STATIC | ACC_PRIVATE) == 0
                            && (!defaults_only || !method.is_abstract())
                        {
                            all.push(m);
                        }
                    }
                }
            }
            if let Some(s) = cls.super_class {
                stack.push(s);
            }
        }
        let classes: Vec<ClassId> = all.iter().map(|&m| self.method(m).class).collect();
        all.into_iter()
            .zip(classes.iter())
            .filter(|&(_, &ic)| !classes.iter().any(|&other| other != ic && self.is_subclass(other, ic)))
            .map(|(m, _)| m)
            .collect()
    }

    /// Selects the method a virtual or interface call runs on a receiver of
    /// class `recv` (JVMS 5.4.6), caching the answer on the class.
    pub fn select(&mut self, recv: ClassId, nd: NameDesc) -> Res<MethodId> {
        if let Some(&m) = self.class(recv).selected.get(&nd) {
            return Ok(m);
        }
        let mut c = Some(recv);
        let mut found = None;
        while let Some(id) = c {
            let cls = self.class(id);
            if let Some(&m) = cls.methods.get(&nd) {
                let method = self.method(m);
                if method.access & ACC_STATIC == 0 {
                    found = Some(m);
                    break;
                }
            }
            c = cls.super_class;
        }
        let m = match found {
            Some(m) => m,
            None => {
                let defaults = self.interface_methods(recv, nd, true);
                match defaults.len() {
                    1 => defaults[0],
                    0 => {
                        let any = self.interface_methods(recv, nd, false);
                        match any.first() {
                            Some(&m) => m,
                            None => {
                                let (n, d) = self.nd_names[nd as usize].clone();
                                let msg = format!("'{}' in {}", self.pretty_method(&n, &d), self.class(recv).java_name());
                                return Err(self.throw_new("java/lang/AbstractMethodError", Some(&msg)));
                            }
                        }
                    }
                    _ => {
                        let (n, d) = self.nd_names[nd as usize].clone();
                        let msg = format!("Conflicting default methods: {n}{d}");
                        return Err(self.throw_new("java/lang/IncompatibleClassChangeError", Some(&msg)));
                    }
                }
            }
        };
        if self.method(m).is_abstract() && !self.class(recv).is_interface() {
            let (n, d) = self.nd_names[nd as usize].clone();
            let msg = format!("Receiver class {} does not define or inherit an implementation of the resolved method '{}' of {}.", self.class(recv).java_name(), self.pretty_method(&n, &d), self.class(self.method(m).class).java_name());
            return Err(self.throw_new("java/lang/AbstractMethodError", Some(&msg)));
        }
        self.classes[recv as usize].selected.insert(nd, m);
        Ok(m)
    }

    /// `abstract void m(int, java.lang.String)`-style text for an error message.
    pub fn pretty_method(&self, name: &str, desc: &str) -> String {
        let Some((params, ret)) = classfile::parse_method_desc(desc) else {
            return format!("{name}{desc}");
        };
        let ty = |d: &str| desc_to_java(d);
        format!(
            "{} {}({})",
            ty(ret),
            name,
            params.iter().map(|p| ty(p)).collect::<Vec<_>>().join(", ")
        )
    }

    // ------------------------------------------------------------- init

    /// Initializes `c` if it is not yet (JVMS 5.5): its superclass and the
    /// superinterfaces that declare default methods first, then its
    /// `ConstantValue` statics and `<clinit>`.
    pub fn ensure_init(&mut self, c: ClassId) -> Res<()> {
        match self.class(c).state {
            InitState::Initialized => return Ok(()),
            InitState::Initializing(_) => return Ok(()),
            InitState::Failed => {
                let msg = format!("Could not initialize class {}", self.class(c).java_name());
                return Err(self.throw_new("java/lang/NoClassDefFoundError", Some(&msg)));
            }
            InitState::Linked => {}
        }
        let thread = self.current_thread;
        self.classes[c as usize].state = InitState::Initializing(thread);
        let result = self.run_init(c);
        match result {
            Ok(()) => {
                self.classes[c as usize].state = InitState::Initialized;
                Ok(())
            }
            Err(Exc::Throw(ex)) => {
                self.classes[c as usize].state = InitState::Failed;
                let error = self.load("java/lang/Error")?;
                if self.instance_of(ex, error) {
                    return Err(Exc::Throw(ex));
                }
                let wrapper = self.throw_new("java/lang/ExceptionInInitializerError", None);
                if let Exc::Throw(w) = &wrapper
                    && let Some(Data::Throwable(t)) = self.obj_mut(*w).map(|o| &mut o.data)
                {
                    t.cause = Some(ex);
                }
                Err(wrapper)
            }
            Err(other) => {
                self.classes[c as usize].state = InitState::Failed;
                Err(other)
            }
        }
    }

    fn run_init(&mut self, c: ClassId) -> Res<()> {
        let cls = self.class(c).clone_shallow();
        if !cls.is_interface {
            if let Some(s) = cls.super_class {
                self.ensure_init(s)?;
            }
            for i in cls.interfaces {
                self.init_default_interfaces(i)?;
            }
        }
        if let ClassKind::Bytecode(cf) = &self.class(c).kind {
            let cf = cf.clone();
            for f in cf.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
                if let Some(index) = f.constant {
                    let value = match cf.get(index) {
                        Ok(classfile::Const::Int(v)) => Value::Int(*v),
                        Ok(classfile::Const::Long(v)) => Value::Long(*v),
                        Ok(classfile::Const::Float(v)) => Value::Float(*v),
                        Ok(classfile::Const::Double(v)) => Value::Double(*v),
                        Ok(classfile::Const::Str(s)) => {
                            let units = cf.utf8(*s).map_err(|e| Exc::Internal(e.0))?.units.clone();
                            Value::Ref(self.intern_units(&units)?)
                        }
                        _ => continue,
                    };
                    let nd = self.nd(&f.name, &f.desc);
                    let slot = self.class(c).static_fields.iter().find(|s| s.nd == nd).map(|s| s.slot);
                    if let Some(slot) = slot {
                        self.classes[c as usize].statics[slot] = value;
                    }
                }
            }
        }
        if let ClassKind::Native(def) = self.class(c).kind
            && let Some(init) = def.clinit
        {
            init(self, &[])?;
        }
        let clinit = self.nd("<clinit>", "()V");
        if let Some(&m) = self.class(c).methods.get(&clinit) {
            self.invoke(m, &[])?;
        }
        Ok(())
    }

    /// Initializes `i` if it declares a default method, and its
    /// superinterfaces that do (JVMS 5.5 step 7).
    fn init_default_interfaces(&mut self, i: ClassId) -> Res<()> {
        let supers = self.class(i).interfaces.clone();
        for s in supers {
            self.init_default_interfaces(s)?;
        }
        let has_default = self.class(i).method_order.iter().any(|&m| {
            let method = self.method(m);
            !method.is_abstract() && method.access & ACC_STATIC == 0
        });
        if has_default {
            self.ensure_init(i)?;
        }
        Ok(())
    }

    /// The `Class` object of `c`, made on first use.
    pub fn mirror(&mut self, c: ClassId) -> Res<ObjId> {
        let existing = self.class(c).mirror;
        if existing != 0 {
            return Ok(existing);
        }
        let class_class = self.known.class;
        let r = self.alloc(Obj {
            class: class_class,
            hash: 0,
            fields: Box::new([]),
            data: Data::Class(c),
        })?;
        self.classes[c as usize].mirror = r;
        self.pinned.push(r);
        Ok(r)
    }

    /// The class a `Class` object stands for.
    pub fn mirror_class(&self, r: ObjId) -> Option<ClassId> {
        match self.obj(r)?.data {
            Data::Class(c) => Some(c),
            _ => None,
        }
    }

    /// Reads a static field by name (for the native library).
    pub fn get_static(&mut self, class: &str, name: &str, desc: &str) -> Res<Value> {
        let c = self.load(class)?;
        self.ensure_init(c)?;
        let nd = self.nd(name, desc);
        match self.resolve_field(c, nd) {
            Some((owner, f, true)) => Ok(self.classes[owner as usize].statics[f.slot]),
            _ => Err(Exc::Internal(format!("no static {class}.{name}"))),
        }
    }

    /// Writes a static field by name (native class initializers use it).
    pub fn set_static(&mut self, class: ClassId, name: &str, value: Value) {
        let cls = &mut self.classes[class as usize];
        if let Some(f) = cls.static_fields.iter().find(|f| &*f.name == name) {
            let slot = f.slot;
            cls.statics[slot] = value;
        }
    }

    /// An instance field's slot by name, looking up the class chain.
    pub fn field_slot(&self, class: ClassId, name: &str) -> Option<usize> {
        let mut c = Some(class);
        while let Some(id) = c {
            let cls = self.class(id);
            if let Some(f) = cls.fields.iter().find(|f| &*f.name == name) {
                return Some(f.slot);
            }
            c = cls.super_class;
        }
        None
    }

    /// Finds a declared or inherited method by name and descriptor.
    pub fn find_method(&mut self, class: ClassId, name: &str, desc: &str) -> Option<MethodId> {
        let nd = self.nd(name, desc);
        self.resolve_method(class, nd)
    }
}

/// The few fields of a class `run_init` needs while it calls into the VM.
struct Shallow {
    is_interface: bool,
    super_class: Option<ClassId>,
    interfaces: Vec<ClassId>,
}

impl Class {
    fn clone_shallow(&self) -> Shallow {
        Shallow {
            is_interface: self.is_interface(),
            super_class: self.super_class,
            interfaces: self.interfaces.clone(),
        }
    }
}

/// A field descriptor as Java source names the type (`int`, `java.lang.String[]`).
pub fn desc_to_java(desc: &str) -> String {
    let dims = desc.bytes().take_while(|&b| b == b'[').count();
    let base = &desc[dims..];
    let name = match base.as_bytes().first() {
        Some(b'L') => base[1..base.len().saturating_sub(1)].replace('/', "."),
        Some(&p) => primitive_name(p).to_string(),
        None => String::new(),
    };
    name + &"[]".repeat(dims)
}

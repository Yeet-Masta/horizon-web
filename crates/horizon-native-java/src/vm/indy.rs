//! `invokedynamic` and dynamic constants: the bootstraps javac emits are the
//! VM's own (lambdas, string concatenation, records' methods, pattern
//! switches, constant bootstraps).
use super::class::{ACC_FINAL, ACC_PUBLIC, ACC_SYNTHETIC, ClassKind, MethodBody};
use super::classfile::{self, ClassFile, Const};
use super::interp::{CallSite, LambdaTarget, Vm};
use super::{Array, ClassId, Data, Exc, MethodId, ObjId, Res, Value};
use std::sync::Arc;

/// One case label of a pattern switch.
#[derive(Debug, Clone)]
pub enum Label {
    Class(ClassId),
    Str(Arc<[u16]>),
    Int(i32),
    /// An enum constant: its class and name.
    Enum(ClassId, Arc<str>),
}

/// A resolved `CONSTANT_MethodHandle`: its kind (JVMS 4.4.8) and member.
#[derive(Debug, Clone)]
pub struct HandleRef {
    pub kind: u8,
    pub class: Arc<str>,
    pub name: Arc<str>,
    pub desc: Arc<str>,
}

pub fn handle_ref(cf: &ClassFile, index: u16) -> Res<HandleRef> {
    match cf.get(index).map_err(|e| Exc::Internal(e.0))? {
        Const::MethodHandle { kind, reference } => {
            let (class, name, desc, _) = cf.member_ref(*reference).map_err(|e| Exc::Internal(e.0))?;
            Ok(HandleRef {
                kind: *kind,
                class,
                name,
                desc,
            })
        }
        other => Err(Exc::Internal(format!("expected a method handle, found {other:?}"))),
    }
}

impl Vm {
    pub(crate) fn resolve_indy(&mut self, m: MethodId, index: u16) -> Res<u32> {
        let cf = self.methods[m as usize].decoded.as_ref().unwrap().cf.clone();
        let (bsm, nt) = match cf.get(index).map_err(|e| Exc::Internal(e.0))? {
            Const::InvokeDynamic { bsm, nt } => (*bsm, *nt),
            other => return Err(Exc::Internal(format!("invokedynamic of {other:?}"))),
        };
        let (site_name, site_desc) = cf.name_and_type(nt).map_err(|e| Exc::Internal(e.0))?;
        let Some(bootstrap) = cf.bootstraps.get(bsm as usize).cloned() else {
            return Err(Exc::Internal("bootstrap method index out of range".into()));
        };
        let bsm_ref = handle_ref(&cf, bootstrap.method)?;
        let caller = self.methods[m as usize].class;
        let site = match (&*bsm_ref.class, &*bsm_ref.name) {
            ("java/lang/invoke/LambdaMetafactory", "metafactory" | "altMetafactory") => {
                self.lambda_site(caller, &cf, &site_name, &site_desc, &bootstrap.args, &bsm_ref.name)?
            }
            ("java/lang/invoke/StringConcatFactory", "makeConcatWithConstants" | "makeConcat") => {
                let (params, _) = classfile::parse_method_desc(&site_desc)
                    .ok_or_else(|| Exc::Internal("bad concat descriptor".into()))?;
                let args: Arc<[Arc<str>]> = params.iter().map(|p| Arc::<str>::from(*p)).collect();
                let (recipe, constants) = if &*bsm_ref.name == "makeConcat" {
                    (vec![1u16; args.len()], Vec::new())
                } else {
                    let mut it = bootstrap.args.iter();
                    let recipe = match it.next().map(|&i| cf.get(i)) {
                        Some(Ok(Const::Str(s))) => cf.utf8(*s).map_err(|e| Exc::Internal(e.0))?.units.to_vec(),
                        _ => return Err(Exc::Internal("concat without a recipe".into())),
                    };
                    let mut constants = Vec::new();
                    for &i in it {
                        let v = self.static_arg(&cf, i)?;
                        let s = self.stringify(v, "Ljava/lang/Object;")?;
                        constants.push(self.new_string_units(s)?);
                    }
                    self.pinned.extend(constants.iter().copied());
                    (recipe, constants)
                };
                CallSite::Concat {
                    recipe: recipe.into(),
                    args,
                    constants: constants.into(),
                }
            }
            ("java/lang/runtime/ObjectMethods", "bootstrap") => {
                let record = match bootstrap.args.first().map(|&i| cf.get(i)) {
                    Some(Ok(Const::Class(_))) => {
                        let name = cf.class_name(bootstrap.args[0]).map_err(|e| Exc::Internal(e.0))?;
                        self.load(&name)?
                    }
                    _ => caller,
                };
                let mut getters = Vec::new();
                for &i in bootstrap.args.iter().skip(2) {
                    let h = handle_ref(&cf, i)?;
                    // A getter handle reads a field: keep it as the field's slot.
                    let c = self.load(&h.class)?;
                    let nd = self.nd(&h.name, &h.desc);
                    let Some((_, f, _)) = self.resolve_field(c, nd) else {
                        return Err(Exc::Internal(format!("record component {} not found", h.name)));
                    };
                    getters.push(f.slot as MethodId);
                    self.jdk.record_descs.insert((record, f.slot), h.desc.clone());
                }
                let kind = match &*site_name {
                    "toString" => 0,
                    "hashCode" => 1,
                    _ => 2,
                };
                CallSite::ObjectMethod {
                    kind,
                    record,
                    getters: getters.into(),
                }
            }
            ("java/lang/runtime/SwitchBootstraps", "typeSwitch" | "enumSwitch") => {
                let mut labels = Vec::new();
                for &i in &bootstrap.args {
                    labels.push(match cf.get(i).map_err(|e| Exc::Internal(e.0))? {
                        Const::Class(_) => {
                            let name = cf.class_name(i).map_err(|e| Exc::Internal(e.0))?;
                            Label::Class(self.class_of_desc_or_name(&name)?)
                        }
                        Const::Str(s) => {
                            let units = cf.utf8(*s).map_err(|e| Exc::Internal(e.0))?.units.clone();
                            if &*bsm_ref.name == "enumSwitch" {
                                // An enum constant's name: the switch's enum class is
                                // the site's first parameter.
                                let (params, _) = classfile::parse_method_desc(&site_desc)
                                    .ok_or_else(|| Exc::Internal("bad switch descriptor".into()))?;
                                let enum_class = self.class_of_desc(params[0])?;
                                Label::Enum(enum_class, String::from_utf16_lossy(&units).into())
                            } else {
                                Label::Str(units)
                            }
                        }
                        Const::Int(v) => Label::Int(*v),
                        Const::Dynamic { .. } => {
                            let v = self.resolve_condy(&cf, i)?;
                            match self.jdk.enum_descs.get(&v.r()).cloned() {
                                Some((c, name)) => Label::Enum(c, name),
                                None => return Err(Exc::Unsupported("switch label constant".into())),
                            }
                        }
                        other => return Err(Exc::Unsupported(format!("switch label {other:?}"))),
                    });
                }
                CallSite::TypeSwitch {
                    labels: labels.into(),
                    enum_switch: &*bsm_ref.name == "enumSwitch",
                }
            }
            (class, name) => {
                return Err(Exc::Unsupported(format!("invokedynamic bootstrap {class}.{name}")));
            }
        };
        self.call_sites.push(site);
        Ok(self.call_sites.len() as u32 - 1)
    }

    /// A static argument of a bootstrap method, as a value.
    fn static_arg(&mut self, cf: &ClassFile, index: u16) -> Res<Value> {
        match cf.get(index).map_err(|e| Exc::Internal(e.0))?.clone() {
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
            Const::Dynamic { .. } => self.resolve_condy(cf, index),
            other => Err(Exc::Unsupported(format!("bootstrap argument {other:?}"))),
        }
    }

    /// A dynamic constant (`CONSTANT_Dynamic`): `ConstantBootstraps` and the
    /// enum descriptors pattern switches use.
    pub(crate) fn resolve_condy(&mut self, cf: &ClassFile, index: u16) -> Res<Value> {
        let key = (Arc::as_ptr(&self.jdk_cf_key(cf)) as *const u8 as usize, index);
        if let Some(&v) = self.jdk.condy_cache.get(&key) {
            return Ok(v);
        }
        let (bsm, nt) = match cf.get(index).map_err(|e| Exc::Internal(e.0))? {
            Const::Dynamic { bsm, nt } => (*bsm, *nt),
            other => return Err(Exc::Internal(format!("dynamic constant {other:?}"))),
        };
        let (name, desc) = cf.name_and_type(nt).map_err(|e| Exc::Internal(e.0))?;
        let bootstrap = cf
            .bootstraps
            .get(bsm as usize)
            .cloned()
            .ok_or_else(|| Exc::Internal("bootstrap index out of range".into()))?;
        let bsm_ref = handle_ref(cf, bootstrap.method)?;
        let value = match (&*bsm_ref.class, &*bsm_ref.name) {
            ("java/lang/invoke/ConstantBootstraps", "nullConstant") => Value::NULL,
            ("java/lang/invoke/ConstantBootstraps", "primitiveClass") => {
                let c = self.primitive_class(name.as_bytes()[0]);
                Value::Ref(self.mirror(c)?)
            }
            ("java/lang/invoke/ConstantBootstraps", "enumConstant") => {
                let c = self.class_of_desc(&desc)?;
                self.ensure_init(c)?;
                let v = self.get_static(&self.class(c).name.to_string(), &name, &desc)?;
                if let Value::Ref(r) = v {
                    self.jdk.enum_descs.insert(r, (c, name.clone()));
                }
                v
            }
            ("java/lang/invoke/ConstantBootstraps", "getStaticFinal") => {
                let owner = match bootstrap.args.first() {
                    Some(&i) => cf.class_name(i).map_err(|e| Exc::Internal(e.0))?.to_string(),
                    None => desc[1..desc.len() - 1].to_string(),
                };
                self.get_static(&owner, &name, &desc)?
            }
            ("java/lang/invoke/ConstantBootstraps", "invoke") => {
                // EnumDesc.of(ClassDesc.of(...), name) and ClassDesc.of(...),
                // as javac emits for enum labels of a pattern switch.
                let h = handle_ref(cf, bootstrap.args[0])?;
                let mut args = Vec::new();
                for &i in &bootstrap.args[1..] {
                    args.push(self.static_arg(cf, i)?);
                }
                super::jdk::invoke::constant_invoke(self, &h, &args)?
            }
            (class, name) => {
                return Err(Exc::Unsupported(format!("dynamic constant bootstrap {class}.{name}")));
            }
        };
        self.jdk.condy_cache.insert(key, value);
        if let Value::Ref(r) = value {
            self.pinned.push(r);
        }
        Ok(value)
    }

    fn jdk_cf_key(&self, cf: &ClassFile) -> Arc<str> {
        cf.name.clone()
    }

    fn lambda_site(
        &mut self,
        caller: ClassId,
        cf: &ClassFile,
        sam_name: &str,
        site_desc: &str,
        args: &[u16],
        factory: &str,
    ) -> Res<CallSite> {
        let (captured, iface_desc) = classfile::parse_method_desc(site_desc)
            .ok_or_else(|| Exc::Internal("bad lambda call site descriptor".into()))?;
        let iface = self.class_of_desc(iface_desc)?;
        let method_type = |vm: &Vm, i: u16| -> Res<Arc<str>> {
            let _ = vm;
            match cf.get(i).map_err(|e| Exc::Internal(e.0))? {
                Const::MethodType(d) => Ok(cf.utf8(*d).map_err(|e| Exc::Internal(e.0))?.text.clone()),
                other => Err(Exc::Internal(format!("expected a method type, found {other:?}"))),
            }
        };
        let sam_desc = method_type(self, args[0])?;
        let impl_ref = handle_ref(cf, args[1])?;
        // The instantiated type (args[2]) only narrows what the erased
        // interface method's arguments are; calls adapt to the target instead.
        method_type(self, args[2])?;
        let mut interfaces = vec![iface];
        let mut bridges: Vec<Arc<str>> = Vec::new();
        if factory == "altMetafactory" {
            let flags = match cf.get(args[3]).map_err(|e| Exc::Internal(e.0))? {
                Const::Int(f) => *f,
                _ => 0,
            };
            let mut at = 4;
            if flags & 2 != 0 {
                let n = match cf.get(args[at]).map_err(|e| Exc::Internal(e.0))? {
                    Const::Int(n) => *n as usize,
                    _ => 0,
                };
                at += 1;
                for &i in args.iter().skip(at).take(n) {
                    let name = cf.class_name(i).map_err(|e| Exc::Internal(e.0))?;
                    interfaces.push(self.load(&name)?);
                }
                at += n;
            }
            if flags & 4 != 0 {
                let n = match args.get(at).map(|&i| cf.get(i)) {
                    Some(Ok(Const::Int(n))) => *n as usize,
                    _ => 0,
                };
                at += 1;
                for &i in args.iter().skip(at).take(n) {
                    bridges.push(method_type(self, i)?);
                }
            }
            if flags & 1 != 0 {
                interfaces.push(self.load("java/io/Serializable")?);
            }
        }
        let impl_class = self.class_of_desc_or_name(&impl_ref.class)?;
        let nd = self.nd(&impl_ref.name, &impl_ref.desc);
        let Some(impl_method) = self.resolve_method(impl_class, nd) else {
            return Err(Exc::Internal(format!("lambda target {}.{} not found", impl_ref.class, impl_ref.name)));
        };
        // The class: one per call site, named as HotSpot names hidden lambda classes.
        let serial = self.jdk.next_lambda;
        self.jdk.next_lambda += 1;
        let name = format!("{}$$Lambda/0x{:016x}", self.class(caller).name, 0x0000_7f00_0000_0000u64 + serial as u64 * 0x418);
        let object = self.known.object;
        let id = self.classes.len() as ClassId;
        let mut class = self.class(object).clone();
        class.id = id;
        class.name = name.as_str().into();
        class.access = ACC_PUBLIC | ACC_FINAL | ACC_SYNTHETIC;
        class.kind = ClassKind::Synthetic;
        class.super_class = Some(object);
        class.interfaces = interfaces;
        class.field_zeros = captured.iter().map(|d| Value::zero(d)).collect::<Vec<_>>().into_boxed_slice();
        class.fields = Vec::new();
        class.static_fields = Vec::new();
        class.statics = Vec::new();
        class.methods = Default::default();
        class.method_order = Vec::new();
        class.selected = Default::default();
        class.state = super::InitState::Initialized;
        class.mirror = 0;
        class.native_root = self.class(object).native_root;
        self.classes.push(class);
        self.class_by_name.insert(name.as_str().into(), id);
        let mut descs = vec![sam_desc.clone()];
        descs.extend(bridges);
        for desc in descs {
            let ret: Arc<str> = classfile::parse_method_desc(&desc)
                .map(|(_, r)| r.into())
                .unwrap_or_else(|| "V".into());
            let index = self.lambdas.len();
            self.lambdas.push(LambdaTarget {
                kind: impl_ref.kind,
                method: impl_method,
                captures: captured.len() as u16,
                sam_ret: ret,
            });
            self.add_method(id, sam_name, &desc, ACC_PUBLIC, MethodBody::Lambda(index), None)?;
        }
        Ok(CallSite::Lambda {
            class: id,
            captures: captured.len() as u16,
            singleton: 0,
        })
    }

    /// Runs a resolved `invokedynamic` against the operand stack.
    pub(crate) fn run_call_site(&mut self, site: usize) -> Res<()> {
        match self.call_sites[site].clone() {
            CallSite::Lambda {
                class,
                captures,
                singleton,
            } => {
                if captures == 0 && singleton != 0 {
                    self.stack.push(Value::Ref(singleton));
                    return Ok(());
                }
                let zeros = self.class(class).field_zeros.len();
                let mut values = Vec::with_capacity(zeros);
                for _ in 0..zeros {
                    let mut v = self.stack.pop().unwrap_or_default();
                    if matches!(v, Value::Top) {
                        v = self.stack.pop().unwrap_or_default();
                    }
                    values.push(v);
                }
                values.reverse();
                let r = self.new_instance(class)?;
                if let Some(o) = self.obj_mut(r) {
                    o.fields = values.into_boxed_slice();
                }
                if captures == 0 {
                    self.pinned.push(r);
                    if let CallSite::Lambda { singleton, .. } = &mut self.call_sites[site] {
                        *singleton = r;
                    }
                }
                self.stack.push(Value::Ref(r));
            }
            CallSite::Concat {
                recipe,
                args,
                constants,
            } => {
                let mut values = Vec::with_capacity(args.len());
                for desc in args.iter().rev() {
                    let v = if matches!(&**desc, "J" | "D") {
                        self.stack.pop();
                        self.stack.pop().unwrap_or_default()
                    } else {
                        self.stack.pop().unwrap_or_default()
                    };
                    values.push(v);
                }
                values.reverse();
                let mut out: Vec<u16> = Vec::new();
                let (mut arg, mut constant) = (0, 0);
                for &unit in recipe.iter() {
                    match unit {
                        1 => {
                            let s = self.stringify(values[arg], &args[arg])?;
                            out.extend_from_slice(&s);
                            arg += 1;
                        }
                        2 => {
                            if let Some(units) = constants.get(constant).and_then(|&c| self.string_units(c)) {
                                out.extend_from_slice(units);
                            }
                            constant += 1;
                        }
                        u => out.push(u),
                    }
                }
                let r = self.new_string_units(out)?;
                self.stack.push(Value::Ref(r));
            }
            CallSite::ObjectMethod {
                kind,
                record,
                getters,
            } => {
                let result = match kind {
                    0 => {
                        let this = self.stack.pop().unwrap_or_default().r();
                        Value::Ref(self.record_to_string(this, record, &getters)?)
                    }
                    1 => {
                        let this = self.stack.pop().unwrap_or_default().r();
                        Value::Int(self.record_hash(this, record, &getters)?)
                    }
                    _ => {
                        let other = self.stack.pop().unwrap_or_default().r();
                        let this = self.stack.pop().unwrap_or_default().r();
                        Value::Int(self.record_equals(this, other, record, &getters)? as i32)
                    }
                };
                self.stack.push(result);
            }
            CallSite::TypeSwitch {
                labels,
                enum_switch,
            } => {
                let restart = self.stack.pop().unwrap_or_default().i();
                let target = self.stack.pop().unwrap_or_default().r();
                let index = self.type_switch(target, restart, &labels, enum_switch)?;
                self.stack.push(Value::Int(index));
            }
        }
        Ok(())
    }

    fn type_switch(&mut self, target: ObjId, restart: i32, labels: &[Label], enum_switch: bool) -> Res<i32> {
        if target == 0 {
            return Ok(-1);
        }
        let _ = enum_switch;
        for (i, label) in labels.iter().enumerate().skip(restart.max(0) as usize) {
            let hit = match label {
                Label::Class(c) => self.instance_of(target, *c),
                Label::Str(units) => self.string_units(target).is_some_and(|u| u == &units[..]),
                Label::Int(v) => match self.obj(target).map(|o| &o.data) {
                    Some(Data::Boxed(b)) => b.i() == *v,
                    _ => false,
                },
                Label::Enum(c, name) => {
                    self.instance_of(target, *c) && super::jdk::lang::enum_name(self, target).as_deref() == Some(&**name)
                }
            };
            if hit {
                return Ok(i as i32);
            }
        }
        Ok(labels.len() as i32)
    }

    fn record_field(&self, this: ObjId, slot: MethodId) -> Value {
        self.obj(this)
            .and_then(|o| o.fields.get(slot as usize))
            .copied()
            .unwrap_or_default()
    }

    fn record_to_string(&mut self, this: ObjId, record: ClassId, slots: &[MethodId]) -> Res<ObjId> {
        let mut out: Vec<u16> = self.simple_name(record).encode_utf16().collect();
        out.push('[' as u16);
        let names: Vec<Arc<str>> = self
            .class(record)
            .class_file()
            .and_then(|cf| cf.record.clone())
            .map(|r| r.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        for (i, &slot) in slots.iter().enumerate() {
            if i > 0 {
                out.extend(", ".encode_utf16());
            }
            if let Some(n) = names.get(i) {
                out.extend(n.encode_utf16());
                out.push('=' as u16);
            }
            let desc = self.jdk.record_descs.get(&(record, slot as usize)).cloned().unwrap_or_else(|| "Ljava/lang/Object;".into());
            let v = self.record_field(this, slot);
            let s = self.stringify(v, &desc)?;
            out.extend_from_slice(&s);
        }
        out.push(']' as u16);
        self.new_string_units(out)
    }

    fn record_hash(&mut self, this: ObjId, record: ClassId, slots: &[MethodId]) -> Res<i32> {
        let mut h: i32 = 0;
        for &slot in slots {
            let desc = self.jdk.record_descs.get(&(record, slot as usize)).cloned().unwrap_or_else(|| "Ljava/lang/Object;".into());
            let v = self.record_field(this, slot);
            let c = super::jdk::lang::hash_of(self, v, &desc)?;
            h = h.wrapping_mul(31).wrapping_add(c);
        }
        Ok(h)
    }

    fn record_equals(&mut self, this: ObjId, other: ObjId, record: ClassId, slots: &[MethodId]) -> Res<bool> {
        if this == other {
            return Ok(true);
        }
        if other == 0 || self.class_of(other) != Some(record) {
            return Ok(false);
        }
        for &slot in slots {
            let desc = self.jdk.record_descs.get(&(record, slot as usize)).cloned().unwrap_or_else(|| "Ljava/lang/Object;".into());
            let (a, b) = (self.record_field(this, slot), self.record_field(other, slot));
            let same = match &*desc {
                "F" => a.f().to_bits() == b.f().to_bits() || (a.f().is_nan() && b.f().is_nan()) || (a.f() == b.f() && a.f() != 0.0),
                "D" => a.d().to_bits() == b.d().to_bits() || (a.d().is_nan() && b.d().is_nan()) || (a.d() == b.d() && a.d() != 0.0),
                d if d.starts_with('L') || d.starts_with('[') => super::jdk::lang::objects_equals(self, a.r(), b.r())?,
                _ => a == b,
            };
            if !same {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `Class.getSimpleName()`.
    pub fn simple_name(&self, c: ClassId) -> String {
        super::jdk::lang::simple_name(self, c)
    }

    /// A value as `String.valueOf` writes it, for its descriptor.
    pub fn stringify(&mut self, v: Value, desc: &str) -> Res<Vec<u16>> {
        let text = match desc.as_bytes().first() {
            Some(b'Z') => (if v.i() != 0 { "true" } else { "false" }).to_string(),
            Some(b'C') => return Ok(vec![v.i() as u16]),
            Some(b'B' | b'S' | b'I') => v.i().to_string(),
            Some(b'J') => v.j().to_string(),
            Some(b'F') => super::java_float_to_string(v.f()),
            Some(b'D') => super::java_double_to_string(v.d()),
            _ => {
                let r = v.r();
                if r == 0 {
                    "null".to_string()
                } else if let Some(units) = self.string_units(r) {
                    return Ok(units.to_vec());
                } else {
                    let s = self.invoke_virtual(r, "toString", "()Ljava/lang/String;", &[])?;
                    return Ok(match self.string_units(s.r()) {
                        Some(u) => u.to_vec(),
                        None => "null".encode_utf16().collect(),
                    });
                }
            }
        };
        Ok(text.encode_utf16().collect())
    }

    /// Runs a lambda class's interface method: the captured values, then the
    /// call's arguments adapted to the target's parameter types.
    pub(crate) fn call_lambda(&mut self, index: usize, args: &[Value]) -> Res<Value> {
        let target = self.lambdas[index].clone();
        let this = args.first().map_or(0, |v| v.r());
        let mut call: Vec<Value> = self
            .obj(this)
            .map(|o| o.fields.iter().copied().take(target.captures as usize).collect())
            .unwrap_or_default();
        call.extend_from_slice(&args[1..]);
        let method = self.method(target.method).clone();
        let (params, ret) = classfile::parse_method_desc(&method.desc)
            .map(|(p, r)| (p.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.to_string()))
            .unwrap_or_default();
        let receiver_first = matches!(target.kind, 5 | 7 | 9) && !method.is_static();
        let offset = receiver_first as usize;
        for (i, p) in params.iter().enumerate() {
            if let Some(v) = call.get_mut(i + offset) {
                *v = self.adapt(*v, p)?;
            }
        }
        let result = match target.kind {
            // newInvokeSpecial: a constructor reference.
            8 => {
                let r = self.new_instance(method.class)?;
                self.ensure_init(method.class)?;
                let mut all = vec![Value::Ref(r)];
                all.extend(call);
                self.invoke(target.method, &all)?;
                Value::Ref(r)
            }
            5 | 9 => {
                let receiver = call.first().map_or(0, |v| v.r());
                let Some(class) = self.class_of(receiver) else {
                    return Err(self.npe(None));
                };
                let m = self.select(class, method.nd)?;
                self.invoke(m, &call)?
            }
            6 => {
                self.ensure_init(method.class)?;
                self.invoke(target.method, &call)?
            }
            _ => self.invoke(target.method, &call)?,
        };
        let produced = if target.kind == 8 { "L".to_string() } else { ret };
        self.adapt_return(result, &produced, &target.sam_ret)
    }

    /// A value converted for a parameter of descriptor `desc`: unboxed for a
    /// primitive parameter, boxed for a reference one.
    pub fn adapt(&mut self, v: Value, desc: &str) -> Res<Value> {
        let primitive = desc.len() == 1;
        match (v, primitive) {
            (Value::Ref(r), true) => super::jdk::lang::unbox(self, r, desc.as_bytes()[0]),
            (Value::Ref(_), false) => Ok(v),
            (_, false) => {
                let kind = match v {
                    Value::Long(_) => b'J',
                    Value::Float(_) => b'F',
                    Value::Double(_) => b'D',
                    _ => match desc {
                        "Ljava/lang/Boolean;" => b'Z',
                        "Ljava/lang/Character;" => b'C',
                        "Ljava/lang/Byte;" => b'B',
                        "Ljava/lang/Short;" => b'S',
                        _ => b'I',
                    },
                };
                Ok(Value::Ref(super::jdk::lang::box_value(self, v, kind)?))
            }
            (v, true) => Ok(super::jdk::lang::widen(v, desc.as_bytes()[0])),
        }
    }

    pub(crate) fn adapt_return(&mut self, v: Value, produced: &str, wanted: &str) -> Res<Value> {
        if wanted == "V" {
            return Ok(Value::Top);
        }
        let produced_primitive = produced.len() == 1 && produced != "V";
        let wanted_primitive = wanted.len() == 1;
        match (produced_primitive, wanted_primitive) {
            (true, false) => Ok(Value::Ref(super::jdk::lang::box_value(self, v, produced.as_bytes()[0])?)),
            (false, true) => super::jdk::lang::unbox(self, v.r(), wanted.as_bytes()[0]),
            (true, true) => Ok(super::jdk::lang::widen(v, wanted.as_bytes()[0])),
            (false, false) => Ok(v),
        }
    }

    /// A copy of an array's elements for the native library.
    pub fn array_values(&self, r: ObjId) -> Vec<Value> {
        match self.array(r) {
            Some(a) => (0..a.len()).filter_map(|i| a.get(i)).collect(),
            None => Vec::new(),
        }
    }

    pub fn ref_array_set(&mut self, r: ObjId, i: usize, v: ObjId) {
        if let Some(Array::L(items)) = self.array_mut(r)
            && let Some(slot) = items.get_mut(i)
        {
            *slot = v;
        }
    }
}

//! Objects, arrays and strings, and the VM-side helpers that make and read
//! them.
use super::interp::Vm;
use super::{ClassId, Exc, ObjId, Res, Value};

/// One object: its class, its identity hash (0 until asked for), its field
/// slots (inherited ones first, so a subclass keeps its parents' slot
/// numbers), and the native state of a JDK class it is, or extends.
#[derive(Debug, Clone)]
pub struct Obj {
    pub class: ClassId,
    pub hash: i32,
    pub fields: Box<[Value]>,
    pub data: Data,
}

#[derive(Debug, Clone)]
pub enum Array {
    Z(Vec<i8>),
    B(Vec<i8>),
    C(Vec<u16>),
    S(Vec<i16>),
    I(Vec<i32>),
    J(Vec<i64>),
    F(Vec<f32>),
    D(Vec<f64>),
    L(Vec<ObjId>),
}

impl Array {
    pub fn len(&self) -> usize {
        match self {
            Array::Z(v) | Array::B(v) => v.len(),
            Array::C(v) => v.len(),
            Array::S(v) => v.len(),
            Array::I(v) => v.len(),
            Array::J(v) => v.len(),
            Array::F(v) => v.len(),
            Array::D(v) => v.len(),
            Array::L(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The element at `i` as a slot value (`None` out of bounds).
    pub fn get(&self, i: usize) -> Option<Value> {
        Some(match self {
            Array::Z(v) | Array::B(v) => Value::Int(*v.get(i)? as i32),
            Array::C(v) => Value::Int(*v.get(i)? as i32),
            Array::S(v) => Value::Int(*v.get(i)? as i32),
            Array::I(v) => Value::Int(*v.get(i)?),
            Array::J(v) => Value::Long(*v.get(i)?),
            Array::F(v) => Value::Float(*v.get(i)?),
            Array::D(v) => Value::Double(*v.get(i)?),
            Array::L(v) => Value::Ref(*v.get(i)?),
        })
    }
    /// Stores `value` at `i`, narrowed as the array's type says; false when out
    /// of bounds.
    pub fn set(&mut self, i: usize, value: Value) -> bool {
        if i >= self.len() {
            return false;
        }
        match self {
            Array::Z(v) => v[i] = (value.i() & 1) as i8,
            Array::B(v) => v[i] = value.i() as i8,
            Array::C(v) => v[i] = value.i() as u16,
            Array::S(v) => v[i] = value.i() as i16,
            Array::I(v) => v[i] = value.i(),
            Array::J(v) => v[i] = value.j(),
            Array::F(v) => v[i] = value.f(),
            Array::D(v) => v[i] = value.d(),
            Array::L(v) => v[i] = value.r(),
        }
        true
    }
    /// A new array of `len` zeros of the element descriptor `elem`.
    pub fn zeroed(elem: u8, len: usize) -> Array {
        match elem {
            b'Z' => Array::Z(vec![0; len]),
            b'B' => Array::B(vec![0; len]),
            b'C' => Array::C(vec![0; len]),
            b'S' => Array::S(vec![0; len]),
            b'I' => Array::I(vec![0; len]),
            b'J' => Array::J(vec![0; len]),
            b'F' => Array::F(vec![0.0; len]),
            b'D' => Array::D(vec![0.0; len]),
            _ => Array::L(vec![0; len]),
        }
    }
}

/// What a `Throwable` carries beyond its fields.
#[derive(Debug, Clone, Default)]
pub struct ThrowableData {
    /// `detailMessage` (a String, or 0).
    pub message: ObjId,
    /// `cause`; `None` until set, as Java's `cause == this`.
    pub cause: Option<ObjId>,
    /// `(method, pc)` of each frame when the trace was filled, innermost first.
    pub frames: Vec<(super::MethodId, u32)>,
    /// A trace a program set (`setStackTrace`), or the one `getStackTrace` made.
    pub trace: ObjId,
    pub suppressed: Vec<ObjId>,
}

/// The native state of an object of a JDK class (or a subclass of one).
#[derive(Debug, Clone, Default)]
pub enum Data {
    #[default]
    None,
    Array(Array),
    /// A `String`'s UTF-16 code units, and its hash once computed (0 until then).
    Str(Box<[u16]>, i32),
    /// A `StringBuilder`'s or `StringBuffer`'s characters.
    Chars(Vec<u16>),
    /// A wrapper's value (`Integer`, `Long`...).
    Boxed(Value),
    /// A `Class` mirror.
    Class(ClassId),
    Throwable(Box<ThrowableData>),
    /// The state of the rest of the native library (collections, streams...).
    Jdk(Box<super::jdk::State>),
}

impl Data {
    /// The references this state holds, for the collector and for copying.
    pub fn refs(&self, out: &mut Vec<ObjId>) {
        match self {
            Data::Array(Array::L(v)) => out.extend(v.iter().copied()),
            Data::Boxed(Value::Ref(r)) => out.push(*r),
            Data::Throwable(t) => {
                out.push(t.message);
                out.extend(t.cause);
                out.push(t.trace);
                out.extend(t.suppressed.iter().copied());
            }
            Data::Jdk(state) => state.refs(out),
            _ => {}
        }
    }
}

impl Vm {
    pub fn obj(&self, r: ObjId) -> Option<&Obj> {
        if r == 0 {
            return None;
        }
        self.objects.get(r as usize).and_then(Option::as_ref)
    }

    pub fn obj_mut(&mut self, r: ObjId) -> Option<&mut Obj> {
        if r == 0 {
            return None;
        }
        self.objects.get_mut(r as usize).and_then(Option::as_mut)
    }

    /// Stores a new object, or throws `OutOfMemoryError` past the heap's cap.
    pub fn alloc(&mut self, obj: Obj) -> Res<ObjId> {
        self.allocated += 1;
        if let Some(slot) = self.free.pop() {
            self.objects[slot as usize] = Some(obj);
            return Ok(slot);
        }
        if self.objects.len() >= self.options.max_objects {
            return Err(self.out_of_memory("Java heap space"));
        }
        let id = self.objects.len() as ObjId;
        self.objects.push(Some(obj));
        Ok(id)
    }

    /// A new instance of a class whose layout is linked, its fields zeroed and
    /// its native state the one its nearest JDK ancestor starts with.
    pub fn new_instance(&mut self, class: ClassId) -> Res<ObjId> {
        let c = &self.classes[class as usize];
        let fields = c.field_zeros.clone();
        let data = super::jdk::initial_data(self, class);
        self.alloc(Obj {
            class,
            hash: 0,
            fields,
            data,
        })
    }

    pub fn class_of(&self, r: ObjId) -> Option<ClassId> {
        self.obj(r).map(|o| o.class)
    }

    // ---------------------------------------------------------------- strings

    /// A new `String` of UTF-16 units (not interned).
    pub fn new_string_units(&mut self, units: Vec<u16>) -> Res<ObjId> {
        let class = self.known.string;
        self.alloc(Obj {
            class,
            hash: 0,
            fields: Box::new([]),
            data: Data::Str(units.into_boxed_slice(), 0),
        })
    }

    pub fn new_string(&mut self, text: &str) -> Res<ObjId> {
        self.new_string_units(text.encode_utf16().collect())
    }

    /// The interned `String` for these units (literals, `intern()`).
    pub fn intern_units(&mut self, units: &[u16]) -> Res<ObjId> {
        if let Some(&r) = self.interned.get(units) {
            return Ok(r);
        }
        let r = self.new_string_units(units.to_vec())?;
        self.interned.insert(units.into(), r);
        Ok(r)
    }

    pub fn intern(&mut self, text: &str) -> Res<ObjId> {
        let units: Vec<u16> = text.encode_utf16().collect();
        self.intern_units(&units)
    }

    /// A `String`'s code units, or `None` for null or a non-string.
    pub fn string_units(&self, r: ObjId) -> Option<&[u16]> {
        match &self.obj(r)?.data {
            Data::Str(units, _) => Some(units),
            _ => None,
        }
    }

    /// A `String`'s text (unpaired surrogates become U+FFFD).
    pub fn string(&self, r: ObjId) -> Option<String> {
        self.string_units(r).map(String::from_utf16_lossy)
    }

    /// A `String`'s text, throwing `NullPointerException` for null.
    pub fn string_nn(&mut self, r: ObjId) -> Res<String> {
        match self.string(r) {
            Some(s) => Ok(s),
            None if r == 0 => Err(self.npe(None)),
            None => Err(Exc::Internal("expected a String".into())),
        }
    }

    // ---------------------------------------------------------------- arrays

    /// A new array of class `class` (`[I`, `[Ljava/lang/String;`...).
    pub fn new_array(&mut self, class: ClassId, len: i32) -> Res<ObjId> {
        if len < 0 {
            return Err(self.throw_new("java/lang/NegativeArraySizeException", Some(&len.to_string())));
        }
        if len as usize > self.options.max_array {
            return Err(self.out_of_memory("Requested array size exceeds VM limit"));
        }
        let elem = self.classes[class as usize]
            .name
            .as_bytes()
            .get(1)
            .copied()
            .unwrap_or(b'L');
        self.alloc(Obj {
            class,
            hash: 0,
            fields: Box::new([]),
            data: Data::Array(Array::zeroed(elem, len as usize)),
        })
    }

    /// A new array of the given element descriptor (`I`, `Ljava/lang/Object;`).
    pub fn new_array_of(&mut self, elem_desc: &str, len: i32) -> Res<ObjId> {
        let class = self.array_class_of_desc(elem_desc)?;
        self.new_array(class, len)
    }

    /// A new `Object[]`-like array holding `items`.
    pub fn new_ref_array(&mut self, elem_desc: &str, items: &[ObjId]) -> Res<ObjId> {
        let r = self.new_array_of(elem_desc, items.len() as i32)?;
        if let Some(Data::Array(Array::L(v))) = self.obj_mut(r).map(|o| &mut o.data) {
            v.copy_from_slice(items);
        }
        Ok(r)
    }

    pub fn array(&self, r: ObjId) -> Option<&Array> {
        match &self.obj(r)?.data {
            Data::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn array_mut(&mut self, r: ObjId) -> Option<&mut Array> {
        match &mut self.obj_mut(r)?.data {
            Data::Array(a) => Some(a),
            _ => None,
        }
    }

    /// The elements of a reference array (empty for null or another kind).
    pub fn ref_elements(&self, r: ObjId) -> Vec<ObjId> {
        match self.array(r) {
            Some(Array::L(v)) => v.clone(),
            _ => Vec::new(),
        }
    }

    // ------------------------------------------------------------ identity

    /// `System.identityHashCode`: from the VM's own seeded xorshift, assigned
    /// on first use, so a run gives the same hashes on any machine.
    pub fn identity_hash(&mut self, r: ObjId) -> i32 {
        if r == 0 {
            return 0;
        }
        if let Some(h) = self.obj(r).map(|o| o.hash).filter(|&h| h != 0) {
            return h;
        }
        let mut h;
        loop {
            let mut x = self.hash_state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.hash_state = x;
            h = (x >> 33) as i32 & 0x7FFF_FFFF;
            if h != 0 {
                break;
            }
        }
        if let Some(o) = self.obj_mut(r) {
            o.hash = h;
        }
        h
    }
}

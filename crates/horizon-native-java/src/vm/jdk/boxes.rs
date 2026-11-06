//! The wrappers (`Integer`, `Long`... with `valueOf`'s caches), `Number`,
//! `Character`, `Boolean`, and `Math`/`StrictMath` (fdlibm's algorithms, from
//! the pure-Rust `libm`, so native and wasm agree to the bit).
use super::{A, ABSTRACT, FINAL, M, NativeClass, NativeMethod, PUBLIC, S, arg, class, ret_bool, ret_ref, ret_str, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Data, Exc, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

pub fn float_to_int_bits(f: f32) -> i32 {
    if f.is_nan() { 0x7fc0_0000 } else { f.to_bits() as i32 }
}

pub fn double_to_long_bits(d: f64) -> i64 {
    if d.is_nan() { 0x7ff8_0000_0000_0000 } else { d.to_bits() as i64 }
}

fn wrapper_class(kind: u8) -> &'static str {
    match kind {
        b'Z' => "java/lang/Boolean",
        b'B' => "java/lang/Byte",
        b'C' => "java/lang/Character",
        b'S' => "java/lang/Short",
        b'J' => "java/lang/Long",
        b'F' => "java/lang/Float",
        b'D' => "java/lang/Double",
        _ => "java/lang/Integer",
    }
}

/// `valueOf`: the caches of `Integer` (-128..127), `Long`, `Short` and `Byte`
/// (the same), `Character` (0..127) and `Boolean`; a new object otherwise.
pub fn box_value(vm: &mut Vm, v: Value, kind: u8) -> Res<ObjId> {
    let v = super::lang::widen(v, kind);
    let v = match kind {
        b'Z' => Value::Int((v.i() != 0) as i32),
        b'B' => Value::Int(v.i() as i8 as i32),
        b'C' => Value::Int(v.i() as u16 as i32),
        b'S' => Value::Int(v.i() as i16 as i32),
        _ => v,
    };
    let cached = match (kind, v) {
        (b'Z', Value::Int(i)) => Some(i as i64),
        (b'C', Value::Int(i)) if (0..128).contains(&i) => Some(i as i64),
        (b'B' | b'S' | b'I', Value::Int(i)) if (-128..128).contains(&i) => Some(i as i64),
        (b'J', Value::Long(l)) if (-128..128).contains(&l) => Some(l),
        _ => None,
    };
    if let Some(key) = cached
        && let Some(&r) = vm.jdk.box_cache.get(&(kind, key))
    {
        return Ok(r);
    }
    let c = vm.load(wrapper_class(kind))?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Boxed(v);
    }
    if let Some(key) = cached {
        vm.jdk.box_cache.insert((kind, key), r);
        vm.pinned.push(r);
    }
    Ok(r)
}

fn boxed(vm: &mut Vm, r: ObjId) -> Res<Value> {
    match vm.obj(r).map(|o| &o.data) {
        Some(Data::Boxed(v)) => Ok(*v),
        _ if r == 0 => Err(vm.npe(None)),
        _ => Err(Exc::Internal("expected a wrapper".into())),
    }
}

fn this_boxed(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    boxed(vm, t)
}

fn nfe(vm: &mut Vm, s: Option<&str>, radix: u32) -> Exc {
    let msg = match s {
        None => "Cannot parse null string: null".to_string(),
        Some(s) if radix == 10 => format!("For input string: \"{s}\""),
        Some(s) => format!("For input string: \"{s}\" under radix {radix}"),
    };
    vm.throw_new("java/lang/NumberFormatException", Some(&msg))
}

/// `Long.parseLong`'s grammar: an optional sign, then digits of the radix.
fn parse_integer(s: &str, radix: u32, min: i128, max: i128) -> Option<i128> {
    if !(2..=36).contains(&radix) || s.is_empty() {
        return None;
    }
    let (neg, digits) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut v: i128 = 0;
    for c in digits.chars() {
        let d = c.to_digit(radix)? as i128;
        v = v * radix as i128 + d;
        if v > max + 1 {
            return None;
        }
    }
    let v = if neg { -v } else { v };
    (min..=max).contains(&v).then_some(v)
}

fn parse_int_arg(vm: &mut Vm, r: ObjId, radix: i32, min: i128, max: i128) -> Res<i128> {
    let s = vm.string(r);
    match &s {
        None => Err(nfe(vm, None, 10)),
        Some(text) => match parse_integer(text, radix as u32, min, max) {
            Some(v) => Ok(v),
            None => Err(nfe(vm, Some(text), radix as u32)),
        },
    }
}

/// `Integer.decode`: `0x`, `#` and leading-zero octal prefixes.
fn decode_integer(s: &str, min: i128, max: i128) -> Option<i128> {
    let (neg, rest) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, digits) = if let Some(h) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")).or_else(|| rest.strip_prefix('#')) {
        (16, h)
    } else if rest.len() > 1 && rest.starts_with('0') {
        (8, &rest[1..])
    } else {
        (10, rest)
    };
    if digits.starts_with('-') || digits.starts_with('+') {
        return None;
    }
    let v = parse_integer(digits, radix, 0, max + 1)?;
    let v = if neg { -v } else { v };
    (min..=max).contains(&v).then_some(v)
}

fn radix_string(v: i64, radix: i32) -> String {
    let radix = if (2..=36).contains(&radix) { radix as u32 } else { 10 };
    if v == 0 {
        return "0".into();
    }
    let neg = v < 0;
    let mut n = (v as i128).unsigned_abs();
    let mut out = Vec::new();
    while n > 0 {
        out.push(std::char::from_digit((n % radix as u128) as u32, radix).unwrap());
        n /= radix as u128;
    }
    if neg {
        out.push('-');
    }
    out.iter().rev().collect()
}

/// `Double.parseDouble`'s grammar: Java float literals, `NaN`, `Infinity`,
/// hex floats, surrounding whitespace allowed.
fn parse_double(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c <= ' ');
    let body = t.trim_end_matches(['d', 'D', 'f', 'F']);
    let (sign, rest) = match body.as_bytes().first()? {
        b'-' => (-1.0, &body[1..]),
        b'+' => (1.0, &body[1..]),
        _ => (1.0, body),
    };
    if rest == "NaN" {
        return Some(f64::NAN);
    }
    if rest == "Infinity" {
        return Some(sign * f64::INFINITY);
    }
    if rest.starts_with("0x") || rest.starts_with("0X") {
        return parse_hex_float(&rest[2..]).map(|v| sign * v);
    }
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        return None;
    }
    let v: f64 = rest.parse().ok()?;
    Some(sign * v)
}

fn parse_hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = s.split_once(['p', 'P'])?;
    let exp: i32 = exp.parse().ok()?;
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let mut v = 0f64;
    for c in int.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(exp))
}

fn double_hex(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sign = if v.is_sign_negative() { "-" } else { "" };
    if v == 0.0 {
        return format!("{sign}0x0.0p0");
    }
    let bits = v.abs().to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let mant = bits & ((1u64 << 52) - 1);
    let m = format!("{mant:013x}");
    let m = m.trim_end_matches('0');
    let m = if m.is_empty() { "0" } else { m };
    if exp == 0 {
        format!("{sign}0x0.{m}p-1022")
    } else {
        format!("{sign}0x1.{m}p{}", exp - 1023)
    }
}

// The wrappers share their instance methods; each value lives in Data::Boxed.

fn int_value(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    Ok(super::lang::widen(this_boxed(vm, a)?, b'I'))
}
fn long_value(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    Ok(super::lang::widen(this_boxed(vm, a)?, b'J'))
}
fn float_value(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let v = this_boxed(vm, a)?;
    Ok(match v {
        Value::Double(d) => Value::Float(d as f32),
        v => super::lang::widen(v, b'F'),
    })
}
fn double_value(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    Ok(super::lang::widen(this_boxed(vm, a)?, b'D'))
}
fn to_int(v: Value) -> i32 {
    match v {
        Value::Int(i) => i,
        Value::Long(l) => l as i32,
        Value::Float(f) => f as i32,
        Value::Double(d) => d as i32,
        _ => 0,
    }
}
fn to_long(v: Value) -> i64 {
    match v {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f as i64,
        Value::Double(d) => d as i64,
        _ => 0,
    }
}
fn short_value(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    Ok(Value::Int(to_int(this_boxed(vm, a)?) as i16 as i32))
}
fn byte_value(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    Ok(Value::Int(to_int(this_boxed(vm, a)?) as i8 as i32))
}

/// `equals` for a wrapper: same class, and the same value (floats by bits).
fn wrapper_equals(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let o = arg(a, 1).r();
    if o == 0 || vm.class_of(o) != vm.class_of(t) {
        return ret_bool(false);
    }
    let (x, y) = (boxed(vm, t)?, boxed(vm, o)?);
    ret_bool(match (x, y) {
        (Value::Float(p), Value::Float(q)) => float_to_int_bits(p) == float_to_int_bits(q),
        (Value::Double(p), Value::Double(q)) => double_to_long_bits(p) == double_to_long_bits(q),
        _ => x == y,
    })
}

fn wrapper_hash(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let v = this_boxed(vm, a)?;
    Ok(Value::Int(hash_value(vm, a[0].r(), v)))
}

fn hash_value(vm: &Vm, r: ObjId, v: Value) -> i32 {
    let class = vm.class_of(r).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
    match v {
        Value::Int(i) if class == "java/lang/Boolean" => {
            if i != 0 { 1231 } else { 1237 }
        }
        Value::Int(i) => i,
        Value::Long(l) => (l ^ (l >> 32)) as i32,
        Value::Float(f) => float_to_int_bits(f),
        Value::Double(d) => {
            let b = double_to_long_bits(d);
            (b ^ (b >> 32)) as i32
        }
        _ => 0,
    }
}

fn float_compare(x: f64, y: f64) -> i32 {
    if x < y {
        -1
    } else if x > y {
        1
    } else {
        let (a, b) = (double_to_long_bits(x), double_to_long_bits(y));
        (a > b) as i32 - (a < b) as i32
    }
}

fn wrapper_compare_to(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let x = this_boxed(vm, a)?;
    let y = boxed(vm, arg(a, 1).r())?;
    Ok(Value::Int(match (x, y) {
        (Value::Long(p), Value::Long(q)) => (p > q) as i32 - (p < q) as i32,
        (Value::Float(p), Value::Float(q)) => {
            let (fa, fb) = (float_to_int_bits(p), float_to_int_bits(q));
            if p < q { -1 } else if p > q { 1 } else { (fa > fb) as i32 - (fa < fb) as i32 }
        }
        (Value::Double(p), Value::Double(q)) => float_compare(p, q),
        (p, q) => {
            let class = vm.class_of(a[0].r()).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
            if class == "java/lang/Byte" || class == "java/lang/Short" || class == "java/lang/Character" {
                p.i() - q.i()
            } else {
                (p.i() > q.i()) as i32 - (p.i() < q.i()) as i32
            }
        }
    }))
}

fn wrapper_to_string(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let v = boxed(vm, t)?;
    let class = vm.class_of(t).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
    let desc = match (class.as_str(), v) {
        ("java/lang/Boolean", _) => "Z",
        ("java/lang/Character", _) => "C",
        (_, Value::Long(_)) => "J",
        (_, Value::Float(_)) => "F",
        (_, Value::Double(_)) => "D",
        _ => "I",
    };
    let u = vm.stringify(v, desc)?;
    ret_ref(vm.new_string_units(u)?)
}

/// The members every integral wrapper has, over its own descriptor letters.
macro_rules! integral {
    ($class:literal, $prim:literal, $kind:expr, $min:expr, $max:expr, $bits:expr) => {
        natives![
            "<init>" (concat!("(", $prim, ")V")) M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(super::lang::widen(arg(a, 1), $kind)); } void() }),
            "<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| { let t = this(vm, a)?; let v = parse_int_arg(vm, arg(a, 1).r(), 10, $min, $max)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(make($kind, v)); } void() }),
            "valueOf" (concat!("(", $prim, ")L", $class, ";")) S => Some(|vm, a| ret_ref(box_value(vm, arg(a, 0), $kind)?)),
            "valueOf" (concat!("(Ljava/lang/String;)L", $class, ";")) S => Some(|vm, a| { let v = parse_int_arg(vm, arg(a, 0).r(), 10, $min, $max)?; ret_ref(box_value(vm, make($kind, v), $kind)?) }),
            "valueOf" (concat!("(Ljava/lang/String;I)L", $class, ";")) S => Some(|vm, a| { let v = parse_int_arg(vm, arg(a, 0).r(), arg(a, 1).i(), $min, $max)?; ret_ref(box_value(vm, make($kind, v), $kind)?) }),
            "decode" (concat!("(Ljava/lang/String;)L", $class, ";")) S => Some(|vm, a| { let s = vm.string_nn(arg(a, 0).r())?; match decode_integer(&s, $min, $max) { Some(v) => ret_ref(box_value(vm, make($kind, v), $kind)?), None => Err(nfe(vm, Some(&s), 10)) } }),
            "intValue" "()I" M => Some(int_value),
            "longValue" "()J" M => Some(long_value),
            "floatValue" "()F" M => Some(float_value),
            "doubleValue" "()D" M => Some(double_value),
            "shortValue" "()S" M => Some(short_value),
            "byteValue" "()B" M => Some(byte_value),
            "equals" "(Ljava/lang/Object;)Z" M => Some(wrapper_equals),
            "hashCode" "()I" M => Some(wrapper_hash),
            "hashCode" (concat!("(", $prim, ")I")) S => Some(|vm, a| { let r = box_value(vm, arg(a, 0), $kind)?; let v = boxed(vm, r)?; Ok(Value::Int(hash_value(vm, r, v))) }),
            "compareTo" (concat!("(L", $class, ";)I")) M => Some(wrapper_compare_to),
            "compareTo" "(Ljava/lang/Object;)I" M => Some(wrapper_compare_to),
            "compare" (concat!("(", $prim, $prim, ")I")) S => Some(|_, a| { let (x, y) = (to_long(arg(a, 0)), to_long(arg(a, 1))); Ok(Value::Int(if $bits >= 32 { (x > y) as i32 - (x < y) as i32 } else { (x - y) as i32 })) }),
            "toString" "()Ljava/lang/String;" M => Some(wrapper_to_string),
            "toString" (concat!("(", $prim, ")Ljava/lang/String;")) S => Some(|vm, a| ret_str(vm, &to_long(arg(a, 0)).to_string())),
            "toString" (concat!("(", $prim, "I)Ljava/lang/String;")) S => Some(|vm, a| ret_str(vm, &radix_string(to_long(arg(a, 0)), arg(a, 1).i()))),
            "sum" (concat!("(", $prim, $prim, ")", $prim)) S => Some(|_, a| Ok(make($kind, to_long(arg(a, 0)) as i128 + to_long(arg(a, 1)) as i128))),
            "max" (concat!("(", $prim, $prim, ")", $prim)) S => Some(|_, a| Ok(make($kind, to_long(arg(a, 0)).max(to_long(arg(a, 1))) as i128))),
            "min" (concat!("(", $prim, $prim, ")", $prim)) S => Some(|_, a| Ok(make($kind, to_long(arg(a, 0)).min(to_long(arg(a, 1))) as i128))),
        ]
    };
}

fn make(kind: u8, v: i128) -> Value {
    match kind {
        b'J' => Value::Long(v as i64),
        b'B' => Value::Int(v as i8 as i32),
        b'S' => Value::Int(v as i16 as i32),
        _ => Value::Int(v as i32),
    }
}

static INTEGER_EXTRA: &[NativeMethod] = natives![
    "parseInt" "(Ljava/lang/String;)I" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), 10, i32::MIN as i128, i32::MAX as i128)? as i32))),
    "parseInt" "(Ljava/lang/String;I)I" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), arg(a, 1).i(), i32::MIN as i128, i32::MAX as i128)? as i32))),
    "parseInt" "(Ljava/lang/CharSequence;III)I" S => Some(|vm, a| { let u = super::string::units(vm, arg(a, 0).r())?; let s = String::from_utf16_lossy(&u[arg(a, 1).i() as usize..arg(a, 2).i() as usize]); match parse_integer(&s, arg(a, 3).i() as u32, i32::MIN as i128, i32::MAX as i128) { Some(v) => Ok(Value::Int(v as i32)), None => Err(nfe(vm, Some(&s), arg(a, 3).i() as u32)) } }),
    "parseUnsignedInt" "(Ljava/lang/String;)I" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), 10, 0, u32::MAX as i128)? as u32 as i32))),
    "parseUnsignedInt" "(Ljava/lang/String;I)I" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), arg(a, 1).i(), 0, u32::MAX as i128)? as u32 as i32))),
    "toHexString" "(I)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &format!("{:x}", arg(a, 0).i() as u32))),
    "toBinaryString" "(I)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &format!("{:b}", arg(a, 0).i() as u32))),
    "toOctalString" "(I)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &format!("{:o}", arg(a, 0).i() as u32))),
    "toUnsignedString" "(I)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &(arg(a, 0).i() as u32).to_string())),
    "toUnsignedString" "(II)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &radix_string(arg(a, 0).i() as u32 as i64, arg(a, 1).i()))),
    "toUnsignedLong" "(I)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).i() as u32 as i64))),
    "bitCount" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().count_ones() as i32))),
    "reverse" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().reverse_bits()))),
    "reverseBytes" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().swap_bytes()))),
    "highestOneBit" "(I)I" S => Some(|_, a| { let v = arg(a, 0).i() as u32; Ok(Value::Int(if v == 0 { 0 } else { (1u32 << (31 - v.leading_zeros())) as i32 })) }),
    "lowestOneBit" "(I)I" S => Some(|_, a| { let v = arg(a, 0).i(); Ok(Value::Int(v & v.wrapping_neg())) }),
    "numberOfLeadingZeros" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().leading_zeros() as i32))),
    "numberOfTrailingZeros" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().trailing_zeros() as i32))),
    "rotateLeft" "(II)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().rotate_left(arg(a, 1).i() as u32 & 31)))),
    "rotateRight" "(II)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().rotate_right(arg(a, 1).i() as u32 & 31)))),
    "signum" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().signum()))),
    "compareUnsigned" "(II)I" S => Some(|_, a| { let (x, y) = (arg(a, 0).i() as u32, arg(a, 1).i() as u32); Ok(Value::Int((x > y) as i32 - (x < y) as i32)) }),
    "divideUnsigned" "(II)I" S => Some(|vm, a| { let y = arg(a, 1).i() as u32; if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } Ok(Value::Int(((arg(a, 0).i() as u32) / y) as i32)) }),
    "remainderUnsigned" "(II)I" S => Some(|vm, a| { let y = arg(a, 1).i() as u32; if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } Ok(Value::Int(((arg(a, 0).i() as u32) % y) as i32)) }),
    "getInteger" "(Ljava/lang/String;)Ljava/lang/Integer;" S => Some(|_, _| ret_ref(0)),
];

static LONG_EXTRA: &[NativeMethod] = natives![
    "parseLong" "(Ljava/lang/String;)J" S => Some(|vm, a| Ok(Value::Long(parse_int_arg(vm, arg(a, 0).r(), 10, i64::MIN as i128, i64::MAX as i128)? as i64))),
    "parseLong" "(Ljava/lang/String;I)J" S => Some(|vm, a| Ok(Value::Long(parse_int_arg(vm, arg(a, 0).r(), arg(a, 1).i(), i64::MIN as i128, i64::MAX as i128)? as i64))),
    "parseUnsignedLong" "(Ljava/lang/String;)J" S => Some(|vm, a| Ok(Value::Long(parse_int_arg(vm, arg(a, 0).r(), 10, 0, u64::MAX as i128)? as u64 as i64))),
    "toHexString" "(J)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &format!("{:x}", arg(a, 0).j() as u64))),
    "toBinaryString" "(J)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &format!("{:b}", arg(a, 0).j() as u64))),
    "toOctalString" "(J)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &format!("{:o}", arg(a, 0).j() as u64))),
    "toUnsignedString" "(J)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &(arg(a, 0).j() as u64).to_string())),
    "bitCount" "(J)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).j().count_ones() as i32))),
    "reverse" "(J)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().reverse_bits()))),
    "reverseBytes" "(J)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().swap_bytes()))),
    "highestOneBit" "(J)J" S => Some(|_, a| { let v = arg(a, 0).j() as u64; Ok(Value::Long(if v == 0 { 0 } else { (1u64 << (63 - v.leading_zeros())) as i64 })) }),
    "lowestOneBit" "(J)J" S => Some(|_, a| { let v = arg(a, 0).j(); Ok(Value::Long(v & v.wrapping_neg())) }),
    "numberOfLeadingZeros" "(J)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).j().leading_zeros() as i32))),
    "numberOfTrailingZeros" "(J)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).j().trailing_zeros() as i32))),
    "rotateLeft" "(JI)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().rotate_left(arg(a, 1).i() as u32 & 63)))),
    "rotateRight" "(JI)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().rotate_right(arg(a, 1).i() as u32 & 63)))),
    "signum" "(J)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).j().signum() as i32))),
    "compareUnsigned" "(JJ)I" S => Some(|_, a| { let (x, y) = (arg(a, 0).j() as u64, arg(a, 1).j() as u64); Ok(Value::Int((x > y) as i32 - (x < y) as i32)) }),
    "divideUnsigned" "(JJ)J" S => Some(|vm, a| { let y = arg(a, 1).j() as u64; if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } Ok(Value::Long(((arg(a, 0).j() as u64) / y) as i64)) }),
    "remainderUnsigned" "(JJ)J" S => Some(|vm, a| { let y = arg(a, 1).j() as u64; if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } Ok(Value::Long(((arg(a, 0).j() as u64) % y) as i64)) }),
];

static SHORT_EXTRA: &[NativeMethod] = natives![
    "parseShort" "(Ljava/lang/String;)S" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), 10, i16::MIN as i128, i16::MAX as i128)? as i32))),
    "parseShort" "(Ljava/lang/String;I)S" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), arg(a, 1).i(), i16::MIN as i128, i16::MAX as i128)? as i32))),
    "reverseBytes" "(S)S" S => Some(|_, a| Ok(Value::Int((arg(a, 0).i() as i16).swap_bytes() as i32))),
    "toUnsignedInt" "(S)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i() as u16 as i32))),
];

static BYTE_EXTRA: &[NativeMethod] = natives![
    "parseByte" "(Ljava/lang/String;)B" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), 10, i8::MIN as i128, i8::MAX as i128)? as i32))),
    "parseByte" "(Ljava/lang/String;I)B" S => Some(|vm, a| Ok(Value::Int(parse_int_arg(vm, arg(a, 0).r(), arg(a, 1).i(), i8::MIN as i128, i8::MAX as i128)? as i32))),
    "toUnsignedInt" "(B)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i() as u8 as i32))),
];

/// A wrapper class, its members being the shared ones plus its own.
const fn wrapper(name: &'static str, methods: &'static [NativeMethod], statics: &'static [(&'static str, &'static str)], clinit: super::NativeFn) -> NativeClass {
    NativeClass {
        name,
        super_name: Some("java/lang/Number"),
        interfaces: &["java/lang/Comparable", "java/lang/constant/Constable"],
        access: PUBLIC | FINAL,
        methods,
        statics,
        clinit: Some(clinit),
        data: Some(|| Data::Boxed(Value::Int(0))),
    }
}

fn set_statics(vm: &mut Vm, class: &str, values: &[(&str, Value)]) -> Res<Value> {
    let c = vm.load(class)?;
    for (name, v) in values {
        vm.set_static(c, name, *v);
    }
    void()
}

fn type_mirror(vm: &mut Vm, p: u8) -> Res<Value> {
    let c = vm.primitive_class(p);
    Ok(Value::Ref(vm.mirror(c)?))
}

static INTEGER: &[NativeMethod] = integral!("java/lang/Integer", "I", b'I', i32::MIN as i128, i32::MAX as i128, 32);
static LONG: &[NativeMethod] = integral!("java/lang/Long", "J", b'J', i64::MIN as i128, i64::MAX as i128, 64);
static SHORT: &[NativeMethod] = integral!("java/lang/Short", "S", b'S', i16::MIN as i128, i16::MAX as i128, 16);
static BYTE: &[NativeMethod] = integral!("java/lang/Byte", "B", b'B', i8::MIN as i128, i8::MAX as i128, 8);

const INT_STATICS: &[(&str, &str)] = &[("MIN_VALUE", "I"), ("MAX_VALUE", "I"), ("TYPE", "Ljava/lang/Class;"), ("SIZE", "I"), ("BYTES", "I")];
const LONG_STATICS: &[(&str, &str)] = &[("MIN_VALUE", "J"), ("MAX_VALUE", "J"), ("TYPE", "Ljava/lang/Class;"), ("SIZE", "I"), ("BYTES", "I")];
const SHORT_STATICS: &[(&str, &str)] = &[("MIN_VALUE", "S"), ("MAX_VALUE", "S"), ("TYPE", "Ljava/lang/Class;"), ("SIZE", "I"), ("BYTES", "I")];
const BYTE_STATICS: &[(&str, &str)] = &[("MIN_VALUE", "B"), ("MAX_VALUE", "B"), ("TYPE", "Ljava/lang/Class;"), ("SIZE", "I"), ("BYTES", "I")];

static CLASSES: &[NativeClass] = &[
    class("java/lang/Number", Some("java/lang/Object"), &["java/io/Serializable"], PUBLIC | ABSTRACT, natives![
        "<init>" "()V" M => Some(|_, _| void()),
        "intValue" "()I" A => None,
        "longValue" "()J" A => None,
        "floatValue" "()F" A => None,
        "doubleValue" "()D" A => None,
        "shortValue" "()S" M => Some(|vm, a| { let t = this(vm, a)?; let v = vm.invoke_virtual(t, "intValue", "()I", &[])?; Ok(Value::Int(v.i() as i16 as i32)) }),
        "byteValue" "()B" M => Some(|vm, a| { let t = this(vm, a)?; let v = vm.invoke_virtual(t, "intValue", "()I", &[])?; Ok(Value::Int(v.i() as i8 as i32)) }),
    ]),
    wrapper("java/lang/Integer", INTEGER, INT_STATICS, |vm, _| { let t = type_mirror(vm, b'I')?; set_statics(vm, "java/lang/Integer", &[("MIN_VALUE", Value::Int(i32::MIN)), ("MAX_VALUE", Value::Int(i32::MAX)), ("TYPE", t), ("SIZE", Value::Int(32)), ("BYTES", Value::Int(4))]) }),
    wrapper("java/lang/Long", LONG, LONG_STATICS, |vm, _| { let t = type_mirror(vm, b'J')?; set_statics(vm, "java/lang/Long", &[("MIN_VALUE", Value::Long(i64::MIN)), ("MAX_VALUE", Value::Long(i64::MAX)), ("TYPE", t), ("SIZE", Value::Int(64)), ("BYTES", Value::Int(8))]) }),
    wrapper("java/lang/Short", SHORT, SHORT_STATICS, |vm, _| { let t = type_mirror(vm, b'S')?; set_statics(vm, "java/lang/Short", &[("MIN_VALUE", Value::Int(i16::MIN as i32)), ("MAX_VALUE", Value::Int(i16::MAX as i32)), ("TYPE", t), ("SIZE", Value::Int(16)), ("BYTES", Value::Int(2))]) }),
    wrapper("java/lang/Byte", BYTE, BYTE_STATICS, |vm, _| { let t = type_mirror(vm, b'B')?; set_statics(vm, "java/lang/Byte", &[("MIN_VALUE", Value::Int(-128)), ("MAX_VALUE", Value::Int(127)), ("TYPE", t), ("SIZE", Value::Int(8)), ("BYTES", Value::Int(1))]) }),
    // The extras: a second description of each wrapper would replace the
    // first, so they hang off helper classes the wrappers' methods name.
    class("java/lang/Integer$Extra", None, &[], 0, INTEGER_EXTRA),
    class("java/lang/Long$Extra", None, &[], 0, LONG_EXTRA),
    class("java/lang/Short$Extra", None, &[], 0, SHORT_EXTRA),
    class("java/lang/Byte$Extra", None, &[], 0, BYTE_EXTRA),
    NativeClass {
        name: "java/lang/Boolean",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable", "java/lang/Comparable", "java/lang/constant/Constable"],
        access: PUBLIC | FINAL,
        methods: natives![
            "<init>" "(Z)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(Value::Int(arg(a, 1).i() & 1)); } void() }),
            "valueOf" "(Z)Ljava/lang/Boolean;" S => Some(|vm, a| ret_ref(box_value(vm, arg(a, 0), b'Z')?)),
            "valueOf" "(Ljava/lang/String;)Ljava/lang/Boolean;" S => Some(|vm, a| { let b = vm.string(arg(a, 0).r()).is_some_and(|s| s.eq_ignore_ascii_case("true")); ret_ref(box_value(vm, Value::Int(b as i32), b'Z')?) }),
            "parseBoolean" "(Ljava/lang/String;)Z" S => Some(|vm, a| ret_bool(vm.string(arg(a, 0).r()).is_some_and(|s| s.eq_ignore_ascii_case("true")))),
            "getBoolean" "(Ljava/lang/String;)Z" S => Some(|vm, a| { let k = vm.string(arg(a, 0).r()).unwrap_or_default(); ret_bool(vm.jdk.properties.iter().any(|(pk, v)| *pk == k && v.eq_ignore_ascii_case("true"))) }),
            "booleanValue" "()Z" M => Some(|vm, a| this_boxed(vm, a)),
            "toString" "()Ljava/lang/String;" M => Some(wrapper_to_string),
            "toString" "(Z)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, if arg(a, 0).z() { "true" } else { "false" })),
            "hashCode" "()I" M => Some(wrapper_hash),
            "hashCode" "(Z)I" S => Some(|_, a| Ok(Value::Int(if arg(a, 0).z() { 1231 } else { 1237 }))),
            "equals" "(Ljava/lang/Object;)Z" M => Some(wrapper_equals),
            "compareTo" "(Ljava/lang/Boolean;)I" M => Some(wrapper_compare_to),
            "compareTo" "(Ljava/lang/Object;)I" M => Some(wrapper_compare_to),
            "compare" "(ZZ)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i() - arg(a, 1).i()))),
            "logicalAnd" "(ZZ)Z" S => Some(|_, a| ret_bool(arg(a, 0).z() && arg(a, 1).z())),
            "logicalOr" "(ZZ)Z" S => Some(|_, a| ret_bool(arg(a, 0).z() || arg(a, 1).z())),
            "logicalXor" "(ZZ)Z" S => Some(|_, a| ret_bool(arg(a, 0).z() != arg(a, 1).z())),
        ],
        statics: &[("TRUE", "Ljava/lang/Boolean;"), ("FALSE", "Ljava/lang/Boolean;"), ("TYPE", "Ljava/lang/Class;")],
        clinit: Some(|vm, _| { let t = box_value(vm, Value::Int(1), b'Z')?; let f = box_value(vm, Value::Int(0), b'Z')?; let ty = type_mirror(vm, b'Z')?; set_statics(vm, "java/lang/Boolean", &[("TRUE", Value::Ref(t)), ("FALSE", Value::Ref(f)), ("TYPE", ty)]) }),
        data: Some(|| Data::Boxed(Value::Int(0))),
    },
    NativeClass {
        name: "java/lang/Character",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable", "java/lang/Comparable", "java/lang/constant/Constable"],
        access: PUBLIC | FINAL,
        methods: CHARACTER,
        statics: &[("MIN_VALUE", "C"), ("MAX_VALUE", "C"), ("TYPE", "Ljava/lang/Class;"), ("MIN_RADIX", "I"), ("MAX_RADIX", "I"), ("MIN_SUPPLEMENTARY_CODE_POINT", "I"), ("MIN_HIGH_SURROGATE", "C"), ("MAX_HIGH_SURROGATE", "C"), ("MIN_LOW_SURROGATE", "C"), ("MAX_LOW_SURROGATE", "C"), ("SIZE", "I"), ("BYTES", "I")],
        clinit: Some(|vm, _| { let ty = type_mirror(vm, b'C')?; set_statics(vm, "java/lang/Character", &[("MIN_VALUE", Value::Int(0)), ("MAX_VALUE", Value::Int(0xffff)), ("TYPE", ty), ("MIN_RADIX", Value::Int(2)), ("MAX_RADIX", Value::Int(36)), ("MIN_SUPPLEMENTARY_CODE_POINT", Value::Int(0x10000)), ("MIN_HIGH_SURROGATE", Value::Int(0xD800)), ("MAX_HIGH_SURROGATE", Value::Int(0xDBFF)), ("MIN_LOW_SURROGATE", Value::Int(0xDC00)), ("MAX_LOW_SURROGATE", Value::Int(0xDFFF)), ("SIZE", Value::Int(16)), ("BYTES", Value::Int(2))]) }),
        data: Some(|| Data::Boxed(Value::Int(0))),
    },
    NativeClass {
        name: "java/lang/Float",
        super_name: Some("java/lang/Number"),
        interfaces: &["java/lang/Comparable", "java/lang/constant/Constable"],
        access: PUBLIC | FINAL,
        methods: FLOAT,
        statics: &[("MIN_VALUE", "F"), ("MAX_VALUE", "F"), ("MIN_NORMAL", "F"), ("NaN", "F"), ("POSITIVE_INFINITY", "F"), ("NEGATIVE_INFINITY", "F"), ("TYPE", "Ljava/lang/Class;"), ("SIZE", "I"), ("BYTES", "I"), ("MAX_EXPONENT", "I"), ("MIN_EXPONENT", "I")],
        clinit: Some(|vm, _| { let ty = type_mirror(vm, b'F')?; set_statics(vm, "java/lang/Float", &[("MIN_VALUE", Value::Float(f32::from_bits(1))), ("MAX_VALUE", Value::Float(f32::MAX)), ("MIN_NORMAL", Value::Float(f32::MIN_POSITIVE)), ("NaN", Value::Float(f32::NAN)), ("POSITIVE_INFINITY", Value::Float(f32::INFINITY)), ("NEGATIVE_INFINITY", Value::Float(f32::NEG_INFINITY)), ("TYPE", ty), ("SIZE", Value::Int(32)), ("BYTES", Value::Int(4)), ("MAX_EXPONENT", Value::Int(127)), ("MIN_EXPONENT", Value::Int(-126))]) }),
        data: Some(|| Data::Boxed(Value::Float(0.0))),
    },
    NativeClass {
        name: "java/lang/Double",
        super_name: Some("java/lang/Number"),
        interfaces: &["java/lang/Comparable", "java/lang/constant/Constable"],
        access: PUBLIC | FINAL,
        methods: DOUBLE,
        statics: &[("MIN_VALUE", "D"), ("MAX_VALUE", "D"), ("MIN_NORMAL", "D"), ("NaN", "D"), ("POSITIVE_INFINITY", "D"), ("NEGATIVE_INFINITY", "D"), ("TYPE", "Ljava/lang/Class;"), ("SIZE", "I"), ("BYTES", "I"), ("MAX_EXPONENT", "I"), ("MIN_EXPONENT", "I")],
        clinit: Some(|vm, _| { let ty = type_mirror(vm, b'D')?; set_statics(vm, "java/lang/Double", &[("MIN_VALUE", Value::Double(f64::from_bits(1))), ("MAX_VALUE", Value::Double(f64::MAX)), ("MIN_NORMAL", Value::Double(f64::MIN_POSITIVE)), ("NaN", Value::Double(f64::NAN)), ("POSITIVE_INFINITY", Value::Double(f64::INFINITY)), ("NEGATIVE_INFINITY", Value::Double(f64::NEG_INFINITY)), ("TYPE", ty), ("SIZE", Value::Int(64)), ("BYTES", Value::Int(8)), ("MAX_EXPONENT", Value::Int(1023)), ("MIN_EXPONENT", Value::Int(-1022))]) }),
        data: Some(|| Data::Boxed(Value::Double(0.0))),
    },
    NativeClass {
        name: "java/lang/Math",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC | FINAL,
        methods: MATH,
        statics: &[("PI", "D"), ("E", "D"), ("TAU", "D")],
        clinit: Some(|vm, _| set_statics(vm, "java/lang/Math", &[("PI", Value::Double(std::f64::consts::PI)), ("E", Value::Double(std::f64::consts::E)), ("TAU", Value::Double(std::f64::consts::TAU))])),
        data: None,
    },
    NativeClass {
        name: "java/lang/StrictMath",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC | FINAL,
        methods: MATH,
        statics: &[("PI", "D"), ("E", "D"), ("TAU", "D")],
        clinit: Some(|vm, _| set_statics(vm, "java/lang/StrictMath", &[("PI", Value::Double(std::f64::consts::PI)), ("E", Value::Double(std::f64::consts::E)), ("TAU", Value::Double(std::f64::consts::TAU))])),
        data: None,
    },
];

fn float_bits(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let _ = vm;
    Ok(Value::Int(float_to_int_bits(arg(a, 0).f())))
}

static FLOAT: &[NativeMethod] = natives![
    "<init>" "(F)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(arg(a, 1)); } void() }),
    "<init>" "(D)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(Value::Float(arg(a, 1).d() as f32)); } void() }),
    "valueOf" "(F)Ljava/lang/Float;" S => Some(|vm, a| ret_ref(box_value(vm, arg(a, 0), b'F')?)),
    "valueOf" "(Ljava/lang/String;)Ljava/lang/Float;" S => Some(|vm, a| { let v = parse_float_arg(vm, arg(a, 0).r())? as f32; ret_ref(box_value(vm, Value::Float(v), b'F')?) }),
    "parseFloat" "(Ljava/lang/String;)F" S => Some(|vm, a| { let s = vm.string_nn(arg(a, 0).r())?; match parse_double(&s) { Some(_) => Ok(Value::Float(parse_f32(&s))), None => Err(float_nfe(vm, &s)) } }),
    "intValue" "()I" M => Some(int_value),
    "longValue" "()J" M => Some(long_value),
    "floatValue" "()F" M => Some(float_value),
    "doubleValue" "()D" M => Some(double_value),
    "shortValue" "()S" M => Some(short_value),
    "byteValue" "()B" M => Some(byte_value),
    "isNaN" "()Z" M => Some(|vm, a| ret_bool(this_boxed(vm, a)?.f().is_nan())),
    "isNaN" "(F)Z" S => Some(|_, a| ret_bool(arg(a, 0).f().is_nan())),
    "isInfinite" "()Z" M => Some(|vm, a| ret_bool(this_boxed(vm, a)?.f().is_infinite())),
    "isInfinite" "(F)Z" S => Some(|_, a| ret_bool(arg(a, 0).f().is_infinite())),
    "isFinite" "(F)Z" S => Some(|_, a| ret_bool(arg(a, 0).f().is_finite())),
    "floatToIntBits" "(F)I" S => Some(float_bits),
    "floatToRawIntBits" "(F)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).f().to_bits() as i32))),
    "intBitsToFloat" "(I)F" S => Some(|_, a| Ok(Value::Float(f32::from_bits(arg(a, 0).i() as u32)))),
    "floatToFloat16" "(F)S" S => Some(|_, _| Err(Exc::Unsupported("Float.floatToFloat16".into()))),
    "toString" "()Ljava/lang/String;" M => Some(wrapper_to_string),
    "toString" "(F)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &crate::vm::java_float_to_string(arg(a, 0).f()))),
    "toHexString" "(F)Ljava/lang/String;" S => Some(|vm, a| { let f = arg(a, 0).f(); let s = if f.is_finite() && f != 0.0 && f.abs() < f32::MIN_POSITIVE { double_hex(f as f64).replace("p-1022", "p-126") } else { double_hex(f as f64) }; ret_str(vm, &s) }),
    "hashCode" "()I" M => Some(wrapper_hash),
    "hashCode" "(F)I" S => Some(float_bits),
    "equals" "(Ljava/lang/Object;)Z" M => Some(wrapper_equals),
    "compareTo" "(Ljava/lang/Float;)I" M => Some(wrapper_compare_to),
    "compareTo" "(Ljava/lang/Object;)I" M => Some(wrapper_compare_to),
    "compare" "(FF)I" S => Some(|_, a| { let (x, y) = (arg(a, 0).f(), arg(a, 1).f()); Ok(Value::Int(if x < y { -1 } else if x > y { 1 } else { let (p, q) = (float_to_int_bits(x), float_to_int_bits(y)); (p > q) as i32 - (p < q) as i32 })) }),
    "sum" "(FF)F" S => Some(|_, a| Ok(Value::Float(arg(a, 0).f() + arg(a, 1).f()))),
    "max" "(FF)F" S => Some(|_, a| Ok(Value::Float(java_max_f(arg(a, 0).f() as f64, arg(a, 1).f() as f64) as f32))),
    "min" "(FF)F" S => Some(|_, a| Ok(Value::Float(java_min_f(arg(a, 0).f() as f64, arg(a, 1).f() as f64) as f32))),
];

static DOUBLE: &[NativeMethod] = natives![
    "<init>" "(D)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(arg(a, 1)); } void() }),
    "valueOf" "(D)Ljava/lang/Double;" S => Some(|vm, a| ret_ref(box_value(vm, arg(a, 0), b'D')?)),
    "valueOf" "(Ljava/lang/String;)Ljava/lang/Double;" S => Some(|vm, a| { let v = parse_float_arg(vm, arg(a, 0).r())?; ret_ref(box_value(vm, Value::Double(v), b'D')?) }),
    "parseDouble" "(Ljava/lang/String;)D" S => Some(|vm, a| Ok(Value::Double(parse_float_arg(vm, arg(a, 0).r())?))),
    "intValue" "()I" M => Some(int_value),
    "longValue" "()J" M => Some(long_value),
    "floatValue" "()F" M => Some(float_value),
    "doubleValue" "()D" M => Some(double_value),
    "shortValue" "()S" M => Some(short_value),
    "byteValue" "()B" M => Some(byte_value),
    "isNaN" "()Z" M => Some(|vm, a| ret_bool(this_boxed(vm, a)?.d().is_nan())),
    "isNaN" "(D)Z" S => Some(|_, a| ret_bool(arg(a, 0).d().is_nan())),
    "isInfinite" "()Z" M => Some(|vm, a| ret_bool(this_boxed(vm, a)?.d().is_infinite())),
    "isInfinite" "(D)Z" S => Some(|_, a| ret_bool(arg(a, 0).d().is_infinite())),
    "isFinite" "(D)Z" S => Some(|_, a| ret_bool(arg(a, 0).d().is_finite())),
    "doubleToLongBits" "(D)J" S => Some(|_, a| Ok(Value::Long(double_to_long_bits(arg(a, 0).d())))),
    "doubleToRawLongBits" "(D)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).d().to_bits() as i64))),
    "longBitsToDouble" "(J)D" S => Some(|_, a| Ok(Value::Double(f64::from_bits(arg(a, 0).j() as u64)))),
    "toString" "()Ljava/lang/String;" M => Some(wrapper_to_string),
    "toString" "(D)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &crate::vm::java_double_to_string(arg(a, 0).d()))),
    "toHexString" "(D)Ljava/lang/String;" S => Some(|vm, a| ret_str(vm, &double_hex(arg(a, 0).d()))),
    "hashCode" "()I" M => Some(wrapper_hash),
    "hashCode" "(D)I" S => Some(|_, a| { let b = double_to_long_bits(arg(a, 0).d()); Ok(Value::Int((b ^ (b >> 32)) as i32)) }),
    "equals" "(Ljava/lang/Object;)Z" M => Some(wrapper_equals),
    "compareTo" "(Ljava/lang/Double;)I" M => Some(wrapper_compare_to),
    "compareTo" "(Ljava/lang/Object;)I" M => Some(wrapper_compare_to),
    "compare" "(DD)I" S => Some(|_, a| Ok(Value::Int(float_compare(arg(a, 0).d(), arg(a, 1).d())))),
    "sum" "(DD)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d() + arg(a, 1).d()))),
    "max" "(DD)D" S => Some(|_, a| Ok(Value::Double(java_max_f(arg(a, 0).d(), arg(a, 1).d())))),
    "min" "(DD)D" S => Some(|_, a| Ok(Value::Double(java_min_f(arg(a, 0).d(), arg(a, 1).d())))),
];

fn float_nfe(vm: &mut Vm, s: &str) -> Exc {
    let msg = if s.trim().is_empty() { "empty String".to_string() } else { format!("For input string: \"{s}\"") };
    vm.throw_new("java/lang/NumberFormatException", Some(&msg))
}

fn parse_float_arg(vm: &mut Vm, r: ObjId) -> Res<f64> {
    let s = vm.string_nn(r)?;
    parse_double(&s).ok_or_else(|| float_nfe(vm, &s))
}

/// `Float.parseFloat`: the decimal rounded once to `float`, not through `double`.
fn parse_f32(s: &str) -> f32 {
    let t = s.trim_matches(|c: char| c <= ' ').trim_end_matches(['d', 'D', 'f', 'F']);
    match t.parse::<f32>() {
        Ok(v) => v,
        Err(_) => parse_double(s).unwrap_or(0.0) as f32,
    }
}

fn java_max_f(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() && b.is_sign_negative() { -0.0 } else { 0.0 }
    } else {
        a.max(b)
    }
}

fn java_min_f(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 }
    } else {
        a.min(b)
    }
}

// ------------------------------------------------------------- Character

fn ch(c: i32) -> Option<char> {
    char::from_u32(c as u32)
}

fn char_map(c: i32, upper: bool) -> i32 {
    match ch(c) {
        Some(x) => {
            let mut m = if upper { x.to_uppercase().collect::<Vec<_>>() } else { x.to_lowercase().collect::<Vec<_>>() };
            if m.len() == 1 { m.remove(0) as i32 } else { c }
        }
        None => c,
    }
}

fn java_digit(c: i32, radix: i32) -> i32 {
    if !(2..=36).contains(&radix) {
        return -1;
    }
    match ch(c).and_then(|x| x.to_digit(radix as u32)) {
        Some(d) => d as i32,
        None => match ch(c) {
            Some(x) if x.is_numeric() => x.to_digit(10).map_or(-1, |d| if (d as i32) < radix { d as i32 } else { -1 }),
            _ => -1,
        },
    }
}

fn java_whitespace(c: i32) -> bool {
    matches!(c, 9..=13 | 0x1c..=0x1f | 0x20 | 0x1680 | 0x2000..=0x2006 | 0x2008..=0x200a | 0x2028 | 0x2029 | 0x205f | 0x3000)
}

static CHARACTER: &[NativeMethod] = natives![
    "<init>" "(C)V" M => Some(|vm, a| { let t = this(vm, a)?; if let Some(o) = vm.obj_mut(t) { o.data = Data::Boxed(arg(a, 1)); } void() }),
    "valueOf" "(C)Ljava/lang/Character;" S => Some(|vm, a| ret_ref(box_value(vm, arg(a, 0), b'C')?)),
    "charValue" "()C" M => Some(|vm, a| this_boxed(vm, a)),
    "hashCode" "()I" M => Some(wrapper_hash),
    "hashCode" "(C)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i()))),
    "equals" "(Ljava/lang/Object;)Z" M => Some(wrapper_equals),
    "compareTo" "(Ljava/lang/Character;)I" M => Some(wrapper_compare_to),
    "compareTo" "(Ljava/lang/Object;)I" M => Some(wrapper_compare_to),
    "compare" "(CC)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i() - arg(a, 1).i()))),
    "toString" "()Ljava/lang/String;" M => Some(wrapper_to_string),
    "toString" "(C)Ljava/lang/String;" S => Some(|vm, a| ret_ref(vm.new_string_units(vec![arg(a, 0).i() as u16])?)),
    "toString" "(I)Ljava/lang/String;" S => Some(|vm, a| { let s: String = ch(arg(a, 0).i()).map(String::from).unwrap_or_default(); ret_str(vm, &s) }),
    "isDigit" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(|c| c.is_ascii_digit() || (c.is_numeric() && c.to_digit(10).is_some())))),
    "isDigit" "(I)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(|c| c.is_ascii_digit()))),
    "isLetter" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_alphabetic))),
    "isLetter" "(I)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_alphabetic))),
    "isLetterOrDigit" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_alphanumeric))),
    "isLetterOrDigit" "(I)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_alphanumeric))),
    "isAlphabetic" "(I)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_alphabetic))),
    "isWhitespace" "(C)Z" S => Some(|_, a| ret_bool(java_whitespace(arg(a, 0).i()))),
    "isWhitespace" "(I)Z" S => Some(|_, a| ret_bool(java_whitespace(arg(a, 0).i()))),
    "isSpaceChar" "(C)Z" S => Some(|_, a| ret_bool(matches!(arg(a, 0).i(), 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000))),
    "isUpperCase" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_uppercase))),
    "isUpperCase" "(I)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_uppercase))),
    "isLowerCase" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_lowercase))),
    "isLowerCase" "(I)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(char::is_lowercase))),
    "isISOControl" "(C)Z" S => Some(|_, a| ret_bool(matches!(arg(a, 0).i(), 0..=0x1f | 0x7f..=0x9f))),
    "isJavaIdentifierStart" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$'))),
    "isJavaIdentifierPart" "(C)Z" S => Some(|_, a| ret_bool(ch(arg(a, 0).i()).is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$'))),
    "toUpperCase" "(C)C" S => Some(|_, a| Ok(Value::Int(char_map(arg(a, 0).i(), true) & 0xffff))),
    "toUpperCase" "(I)I" S => Some(|_, a| Ok(Value::Int(char_map(arg(a, 0).i(), true)))),
    "toLowerCase" "(C)C" S => Some(|_, a| Ok(Value::Int(char_map(arg(a, 0).i(), false) & 0xffff))),
    "toLowerCase" "(I)I" S => Some(|_, a| Ok(Value::Int(char_map(arg(a, 0).i(), false)))),
    "toTitleCase" "(C)C" S => Some(|_, a| Ok(Value::Int(char_map(arg(a, 0).i(), true) & 0xffff))),
    "digit" "(CI)I" S => Some(|_, a| Ok(Value::Int(java_digit(arg(a, 0).i(), arg(a, 1).i())))),
    "digit" "(II)I" S => Some(|_, a| Ok(Value::Int(java_digit(arg(a, 0).i(), arg(a, 1).i())))),
    "getNumericValue" "(C)I" S => Some(|_, a| Ok(Value::Int(match ch(arg(a, 0).i()) { Some(c) if c.is_ascii_alphanumeric() => c.to_digit(36).map_or(-1, |d| d as i32), Some(c) => c.to_digit(10).map_or(-1, |d| d as i32), None => -1 }))),
    "getNumericValue" "(I)I" S => Some(|_, a| Ok(Value::Int(ch(arg(a, 0).i()).and_then(|c| c.to_digit(36)).map_or(-1, |d| d as i32)))),
    "forDigit" "(II)C" S => Some(|_, a| Ok(Value::Int(std::char::from_digit(arg(a, 0).i() as u32, arg(a, 1).i() as u32).map_or(0, |c| c as i32)))),
    "charCount" "(I)I" S => Some(|_, a| Ok(Value::Int(if arg(a, 0).i() >= 0x10000 { 2 } else { 1 }))),
    "isSurrogate" "(C)Z" S => Some(|_, a| ret_bool((0xD800..0xE000).contains(&arg(a, 0).i()))),
    "isHighSurrogate" "(C)Z" S => Some(|_, a| ret_bool((0xD800..0xDC00).contains(&arg(a, 0).i()))),
    "isLowSurrogate" "(C)Z" S => Some(|_, a| ret_bool((0xDC00..0xE000).contains(&arg(a, 0).i()))),
    "isSupplementaryCodePoint" "(I)Z" S => Some(|_, a| ret_bool((0x10000..=0x10FFFF).contains(&arg(a, 0).i()))),
    "isBmpCodePoint" "(I)Z" S => Some(|_, a| ret_bool((0..0x10000).contains(&arg(a, 0).i()))),
    "isValidCodePoint" "(I)Z" S => Some(|_, a| ret_bool((0..=0x10FFFF).contains(&arg(a, 0).i()))),
    "toCodePoint" "(CC)I" S => Some(|_, a| Ok(Value::Int(0x10000 + ((arg(a, 0).i() - 0xD800) << 10) + (arg(a, 1).i() - 0xDC00)))),
    "highSurrogate" "(I)C" S => Some(|_, a| Ok(Value::Int(((arg(a, 0).i() - 0x10000) >> 10) + 0xD800))),
    "lowSurrogate" "(I)C" S => Some(|_, a| Ok(Value::Int(((arg(a, 0).i() - 0x10000) & 0x3ff) + 0xDC00))),
    "toChars" "(I)[C" S => Some(|vm, a| { let c = arg(a, 0).i(); let u: Vec<u16> = match ch(c) { Some(x) => { let mut b = [0u16; 2]; x.encode_utf16(&mut b).to_vec() } None => vec![c as u16] }; let r = vm.new_array_of("C", u.len() as i32)?; if let Some(crate::vm::Array::C(v)) = vm.array_mut(r) { v.copy_from_slice(&u); } ret_ref(r) }),
    "reverseBytes" "(C)C" S => Some(|_, a| Ok(Value::Int((arg(a, 0).i() as u16).swap_bytes() as i32))),
    "getType" "(C)I" S => Some(|_, a| Ok(Value::Int(match ch(arg(a, 0).i()) { Some(c) if c.is_uppercase() => 1, Some(c) if c.is_lowercase() => 2, Some(c) if c.is_ascii_digit() => 9, Some(' ') => 12, Some(c) if c.is_control() => 15, _ => 0 }))),
];

// ------------------------------------------------------------------ Math

fn ulp(d: f64) -> f64 {
    if d.is_nan() {
        return f64::NAN;
    }
    if d.is_infinite() {
        return f64::INFINITY;
    }
    let a = d.abs();
    if a == f64::MAX {
        return 2f64.powi(971);
    }
    f64::from_bits(a.to_bits() + 1) - a
}

fn ulp_f(f: f32) -> f32 {
    if f.is_nan() {
        return f32::NAN;
    }
    if f.is_infinite() {
        return f32::INFINITY;
    }
    let a = f.abs();
    if a == f32::MAX {
        return 2f32.powi(104);
    }
    f32::from_bits(a.to_bits() + 1) - a
}

fn next_after(start: f64, direction: f64) -> f64 {
    if start.is_nan() || direction.is_nan() {
        return f64::NAN;
    }
    if start == direction {
        return direction;
    }
    if start == 0.0 {
        return if direction > 0.0 { f64::from_bits(1) } else { -f64::from_bits(1) };
    }
    let bits = start.to_bits();
    let up = (direction > start) == (start > 0.0);
    f64::from_bits(if up { bits + 1 } else { bits - 1 })
}

fn next_after_f(start: f32, direction: f64) -> f32 {
    if start.is_nan() || direction.is_nan() {
        return f32::NAN;
    }
    if start as f64 == direction {
        return direction as f32;
    }
    if start == 0.0 {
        return if direction > 0.0 { f32::from_bits(1) } else { -f32::from_bits(1) };
    }
    let bits = start.to_bits();
    let up = (direction > start as f64) == (start > 0.0);
    f32::from_bits(if up { bits + 1 } else { bits - 1 })
}

fn exact_error(vm: &mut Vm, what: &str) -> Exc {
    vm.throw_new("java/lang/ArithmeticException", Some(&format!("{what} overflow")))
}

fn rint(x: f64) -> f64 {
    libm::rint(x)
}

/// `Math.round(double)`: floor(x + 0.5) without the double rounding error,
/// saturating, NaN to 0.
fn round_d(x: f64) -> i64 {
    if x.is_nan() {
        return 0;
    }
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    r as i64
}

fn round_f(x: f32) -> i32 {
    if x.is_nan() {
        return 0;
    }
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    r as i32
}

static MATH: &[NativeMethod] = natives![
    "abs" "(I)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().wrapping_abs()))),
    "abs" "(J)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().wrapping_abs()))),
    "abs" "(F)F" S => Some(|_, a| Ok(Value::Float(arg(a, 0).f().abs()))),
    "abs" "(D)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d().abs()))),
    "absExact" "(I)I" S => Some(|vm, a| { let v = arg(a, 0).i(); if v == i32::MIN { return Err(vm.throw_new("java/lang/ArithmeticException", Some("Overflow to represent absolute value of Integer.MIN_VALUE"))); } Ok(Value::Int(v.abs())) }),
    "absExact" "(J)J" S => Some(|vm, a| { let v = arg(a, 0).j(); if v == i64::MIN { return Err(vm.throw_new("java/lang/ArithmeticException", Some("Overflow to represent absolute value of Long.MIN_VALUE"))); } Ok(Value::Long(v.abs())) }),
    "max" "(II)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().max(arg(a, 1).i())))),
    "max" "(JJ)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().max(arg(a, 1).j())))),
    "max" "(FF)F" S => Some(|_, a| Ok(Value::Float(java_max_f(arg(a, 0).f() as f64, arg(a, 1).f() as f64) as f32))),
    "max" "(DD)D" S => Some(|_, a| Ok(Value::Double(java_max_f(arg(a, 0).d(), arg(a, 1).d())))),
    "min" "(II)I" S => Some(|_, a| Ok(Value::Int(arg(a, 0).i().min(arg(a, 1).i())))),
    "min" "(JJ)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).j().min(arg(a, 1).j())))),
    "min" "(FF)F" S => Some(|_, a| Ok(Value::Float(java_min_f(arg(a, 0).f() as f64, arg(a, 1).f() as f64) as f32))),
    "min" "(DD)D" S => Some(|_, a| Ok(Value::Double(java_min_f(arg(a, 0).d(), arg(a, 1).d())))),
    "clamp" "(JII)I" S => Some(|vm, a| { let (v, lo, hi) = (arg(a, 0).j(), arg(a, 1).i(), arg(a, 2).i()); if lo > hi { let msg = format!("{lo} > {hi}"); return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg))); } Ok(Value::Int(v.clamp(lo as i64, hi as i64) as i32)) }),
    "clamp" "(JJJ)J" S => Some(|vm, a| { let (v, lo, hi) = (arg(a, 0).j(), arg(a, 1).j(), arg(a, 2).j()); if lo > hi { let msg = format!("{lo} > {hi}"); return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg))); } Ok(Value::Long(v.clamp(lo, hi))) }),
    "clamp" "(DDD)D" S => Some(|vm, a| { let (v, lo, hi) = (arg(a, 0).d(), arg(a, 1).d(), arg(a, 2).d()); if lo > hi || lo.is_nan() || hi.is_nan() { let msg = format!("{} > {}", crate::vm::java_double_to_string(lo), crate::vm::java_double_to_string(hi)); return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg))); } Ok(Value::Double(java_min_f(hi, java_max_f(v, lo)))) }),
    "clamp" "(FFF)F" S => Some(|_, a| { let (v, lo, hi) = (arg(a, 0).f() as f64, arg(a, 1).f() as f64, arg(a, 2).f() as f64); Ok(Value::Float(java_min_f(hi, java_max_f(v, lo)) as f32)) }),
    "sqrt" "(D)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d().sqrt()))),
    "cbrt" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::cbrt(arg(a, 0).d())))),
    "pow" "(DD)D" S => Some(|_, a| Ok(Value::Double(libm::pow(arg(a, 0).d(), arg(a, 1).d())))),
    "exp" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::exp(arg(a, 0).d())))),
    "expm1" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::expm1(arg(a, 0).d())))),
    "log" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::log(arg(a, 0).d())))),
    "log10" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::log10(arg(a, 0).d())))),
    "log1p" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::log1p(arg(a, 0).d())))),
    "sin" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::sin(arg(a, 0).d())))),
    "cos" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::cos(arg(a, 0).d())))),
    "tan" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::tan(arg(a, 0).d())))),
    "asin" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::asin(arg(a, 0).d())))),
    "acos" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::acos(arg(a, 0).d())))),
    "atan" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::atan(arg(a, 0).d())))),
    "atan2" "(DD)D" S => Some(|_, a| Ok(Value::Double(libm::atan2(arg(a, 0).d(), arg(a, 1).d())))),
    "sinh" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::sinh(arg(a, 0).d())))),
    "cosh" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::cosh(arg(a, 0).d())))),
    "tanh" "(D)D" S => Some(|_, a| Ok(Value::Double(libm::tanh(arg(a, 0).d())))),
    "hypot" "(DD)D" S => Some(|_, a| Ok(Value::Double(libm::hypot(arg(a, 0).d(), arg(a, 1).d())))),
    "toRadians" "(D)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d() * 0.017453292519943295))),
    "toDegrees" "(D)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d() * 57.29577951308232))),
    "floor" "(D)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d().floor()))),
    "ceil" "(D)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d().ceil()))),
    "rint" "(D)D" S => Some(|_, a| Ok(Value::Double(rint(arg(a, 0).d())))),
    "round" "(D)J" S => Some(|_, a| Ok(Value::Long(round_d(arg(a, 0).d())))),
    "round" "(F)I" S => Some(|_, a| Ok(Value::Int(round_f(arg(a, 0).f())))),
    "signum" "(D)D" S => Some(|_, a| { let d = arg(a, 0).d(); Ok(Value::Double(if d == 0.0 || d.is_nan() { d } else { d.signum() })) }),
    "signum" "(F)F" S => Some(|_, a| { let d = arg(a, 0).f(); Ok(Value::Float(if d == 0.0 || d.is_nan() { d } else { d.signum() })) }),
    "ulp" "(D)D" S => Some(|_, a| Ok(Value::Double(ulp(arg(a, 0).d())))),
    "ulp" "(F)F" S => Some(|_, a| Ok(Value::Float(ulp_f(arg(a, 0).f())))),
    "nextUp" "(D)D" S => Some(|_, a| Ok(Value::Double(next_after(arg(a, 0).d(), f64::INFINITY)))),
    "nextUp" "(F)F" S => Some(|_, a| Ok(Value::Float(next_after_f(arg(a, 0).f(), f64::INFINITY)))),
    "nextDown" "(D)D" S => Some(|_, a| Ok(Value::Double(next_after(arg(a, 0).d(), f64::NEG_INFINITY)))),
    "nextAfter" "(DD)D" S => Some(|_, a| Ok(Value::Double(next_after(arg(a, 0).d(), arg(a, 1).d())))),
    "nextAfter" "(FD)F" S => Some(|_, a| Ok(Value::Float(next_after_f(arg(a, 0).f(), arg(a, 1).d())))),
    "copySign" "(DD)D" S => Some(|_, a| Ok(Value::Double(arg(a, 0).d().copysign(arg(a, 1).d())))),
    "copySign" "(FF)F" S => Some(|_, a| Ok(Value::Float(arg(a, 0).f().copysign(arg(a, 1).f())))),
    "getExponent" "(D)I" S => Some(|_, a| Ok(Value::Int(((arg(a, 0).d().to_bits() >> 52) & 0x7ff) as i32 - 1023))),
    "getExponent" "(F)I" S => Some(|_, a| Ok(Value::Int(((arg(a, 0).f().to_bits() >> 23) & 0xff) as i32 - 127))),
    "scalb" "(DI)D" S => Some(|_, a| Ok(Value::Double(libm::scalbn(arg(a, 0).d(), arg(a, 1).i())))),
    "scalb" "(FI)F" S => Some(|_, a| Ok(Value::Float(libm::scalbnf(arg(a, 0).f(), arg(a, 1).i())))),
    "IEEEremainder" "(DD)D" S => Some(|_, a| Ok(Value::Double(libm::remainder(arg(a, 0).d(), arg(a, 1).d())))),
    "fma" "(DDD)D" S => Some(|_, a| Ok(Value::Double(libm::fma(arg(a, 0).d(), arg(a, 1).d(), arg(a, 2).d())))),
    "fma" "(FFF)F" S => Some(|_, a| Ok(Value::Float(libm::fmaf(arg(a, 0).f(), arg(a, 1).f(), arg(a, 2).f())))),
    "random" "()D" S => Some(|vm, _| { let r = super::util::shared_random(vm)?; super::util::random_next_double(vm, r) }),
    "addExact" "(II)I" S => Some(|vm, a| arg(a, 0).i().checked_add(arg(a, 1).i()).map(Value::Int).ok_or_else(|| exact_error(vm, "integer"))),
    "addExact" "(JJ)J" S => Some(|vm, a| arg(a, 0).j().checked_add(arg(a, 1).j()).map(Value::Long).ok_or_else(|| exact_error(vm, "long"))),
    "subtractExact" "(II)I" S => Some(|vm, a| arg(a, 0).i().checked_sub(arg(a, 1).i()).map(Value::Int).ok_or_else(|| exact_error(vm, "integer"))),
    "subtractExact" "(JJ)J" S => Some(|vm, a| arg(a, 0).j().checked_sub(arg(a, 1).j()).map(Value::Long).ok_or_else(|| exact_error(vm, "long"))),
    "multiplyExact" "(II)I" S => Some(|vm, a| arg(a, 0).i().checked_mul(arg(a, 1).i()).map(Value::Int).ok_or_else(|| exact_error(vm, "integer"))),
    "multiplyExact" "(JJ)J" S => Some(|vm, a| arg(a, 0).j().checked_mul(arg(a, 1).j()).map(Value::Long).ok_or_else(|| exact_error(vm, "long"))),
    "multiplyExact" "(JI)J" S => Some(|vm, a| arg(a, 0).j().checked_mul(arg(a, 1).i() as i64).map(Value::Long).ok_or_else(|| exact_error(vm, "long"))),
    "negateExact" "(I)I" S => Some(|vm, a| arg(a, 0).i().checked_neg().map(Value::Int).ok_or_else(|| exact_error(vm, "integer"))),
    "negateExact" "(J)J" S => Some(|vm, a| arg(a, 0).j().checked_neg().map(Value::Long).ok_or_else(|| exact_error(vm, "long"))),
    "incrementExact" "(I)I" S => Some(|vm, a| arg(a, 0).i().checked_add(1).map(Value::Int).ok_or_else(|| exact_error(vm, "integer"))),
    "decrementExact" "(I)I" S => Some(|vm, a| arg(a, 0).i().checked_sub(1).map(Value::Int).ok_or_else(|| exact_error(vm, "integer"))),
    "toIntExact" "(J)I" S => Some(|vm, a| i32::try_from(arg(a, 0).j()).map(Value::Int).map_err(|_| exact_error(vm, "integer"))),
    "multiplyHigh" "(JJ)J" S => Some(|_, a| Ok(Value::Long(((arg(a, 0).j() as i128 * arg(a, 1).j() as i128) >> 64) as i64))),
    "multiplyFull" "(II)J" S => Some(|_, a| Ok(Value::Long(arg(a, 0).i() as i64 * arg(a, 1).i() as i64))),
    "floorDiv" "(II)I" S => Some(|vm, a| { let (x, y) = (arg(a, 0).i(), arg(a, 1).i()); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let q = x.wrapping_div(y); Ok(Value::Int(if (x % y != 0) && ((x < 0) != (y < 0)) { q - 1 } else { q })) }),
    "floorDiv" "(JJ)J" S => Some(|vm, a| { let (x, y) = (arg(a, 0).j(), arg(a, 1).j()); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let q = x.wrapping_div(y); Ok(Value::Long(if (x % y != 0) && ((x < 0) != (y < 0)) { q - 1 } else { q })) }),
    "floorDiv" "(JI)J" S => Some(|vm, a| { let (x, y) = (arg(a, 0).j(), arg(a, 1).i() as i64); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let q = x.wrapping_div(y); Ok(Value::Long(if (x % y != 0) && ((x < 0) != (y < 0)) { q - 1 } else { q })) }),
    "floorMod" "(II)I" S => Some(|vm, a| { let (x, y) = (arg(a, 0).i(), arg(a, 1).i()); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let m = x.wrapping_rem(y); Ok(Value::Int(if m != 0 && ((m < 0) != (y < 0)) { m + y } else { m })) }),
    "floorMod" "(JJ)J" S => Some(|vm, a| { let (x, y) = (arg(a, 0).j(), arg(a, 1).j()); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let m = x.wrapping_rem(y); Ok(Value::Long(if m != 0 && ((m < 0) != (y < 0)) { m + y } else { m })) }),
    "floorMod" "(JI)I" S => Some(|vm, a| { let (x, y) = (arg(a, 0).j(), arg(a, 1).i() as i64); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let m = x.wrapping_rem(y); Ok(Value::Int((if m != 0 && ((m < 0) != (y < 0)) { m + y } else { m }) as i32)) }),
    "ceilDiv" "(II)I" S => Some(|vm, a| { let (x, y) = (arg(a, 0).i(), arg(a, 1).i()); if y == 0 { return Err(vm.throw_new("java/lang/ArithmeticException", Some("/ by zero"))); } let q = x.wrapping_div(y); Ok(Value::Int(if (x % y != 0) && ((x < 0) == (y < 0)) { q + 1 } else { q })) }),
];

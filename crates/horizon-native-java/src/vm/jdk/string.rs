//! `String`, `StringBuilder`/`StringBuffer` and `String.format`.
use super::{FINAL, M, NativeClass, NativeMethod, PUBLIC, S, arg, class, ret_bool, ret_ref, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Array, Data, Exc, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

/// `String.hashCode`: `s[0]*31^(n-1) + ... + s[n-1]`.
pub fn java_hash(units: &[u16]) -> i32 {
    units.iter().fold(0i32, |h, &u| h.wrapping_mul(31).wrapping_add(u as i32))
}

/// A string argument's units, `NullPointerException` for null.
pub fn units(vm: &mut Vm, r: ObjId) -> Res<Vec<u16>> {
    match vm.string_units(r) {
        Some(u) => Ok(u.to_vec()),
        None if r == 0 => Err(vm.npe(None)),
        None => {
            // A CharSequence that is not a String: its toString().
            let s = vm.invoke_virtual(r, "toString", "()Ljava/lang/String;", &[])?.r();
            Ok(vm.string_units(s).map(<[u16]>::to_vec).unwrap_or_default())
        }
    }
}

fn this_units(vm: &mut Vm, a: &[Value]) -> Res<Vec<u16>> {
    let t = this(vm, a)?;
    units(vm, t)
}

fn new_str(vm: &mut Vm, u: Vec<u16>) -> Res<Value> {
    Ok(Value::Ref(vm.new_string_units(u)?))
}

fn sioobe(vm: &mut Vm, msg: String) -> Exc {
    vm.throw_new("java/lang/StringIndexOutOfBoundsException", Some(&msg))
}

fn index_of(hay: &[u16], needle: &[u16], from: usize) -> i32 {
    if needle.is_empty() {
        return from.min(hay.len()) as i32;
    }
    if from >= hay.len() || needle.len() > hay.len() {
        return -1;
    }
    (from..=hay.len() - needle.len())
        .find(|&i| &hay[i..i + needle.len()] == needle)
        .map_or(-1, |i| i as i32)
}

fn last_index_of(hay: &[u16], needle: &[u16], from: i32) -> i32 {
    if from < 0 || needle.len() > hay.len() {
        return -1;
    }
    let start = (from as usize).min(hay.len() - needle.len());
    (0..=start)
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
        .map_or(-1, |i| i as i32)
}

fn char_units(c: i32) -> Vec<u16> {
    match char::from_u32(c as u32) {
        Some(ch) => {
            let mut b = [0u16; 2];
            ch.encode_utf16(&mut b).to_vec()
        }
        None => vec![c as u16],
    }
}

fn is_java_whitespace(u: u16) -> bool {
    matches!(u, 9..=13 | 0x1c..=0x1f | 0x20 | 0x1680 | 0x2000..=0x2006 | 0x2008..=0x200a | 0x2028 | 0x2029 | 0x205f | 0x3000)
}

fn to_string_lossless(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

fn compare(a: &[u16], b: &[u16]) -> i32 {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return *x as i32 - *y as i32;
        }
    }
    a.len() as i32 - b.len() as i32
}

fn fold_char(c: u16) -> u16 {
    let up = char::from_u32(c as u32)
        .map(|ch| {
            let mut u = ch.to_uppercase();
            if u.len() == 1 { u.next().unwrap() } else { ch }
        })
        .unwrap_or(c as u8 as char);
    let low = up.to_lowercase();
    if low.len() == 1 {
        let l = low.into_iter().next().unwrap();
        if (l as u32) < 0x10000 {
            return l as u16;
        }
    }
    c
}

fn compare_ignore_case(a: &[u16], b: &[u16]) -> i32 {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            let (fx, fy) = (fold_char(*x), fold_char(*y));
            if fx != fy {
                return fx as i32 - fy as i32;
            }
        }
    }
    a.len() as i32 - b.len() as i32
}

fn case_map(u: &[u16], upper: bool) -> Vec<u16> {
    let s = to_string_lossless(u);
    let mapped = if upper { s.to_uppercase() } else { s.to_lowercase() };
    mapped.encode_utf16().collect()
}

fn java_trim(u: &[u16]) -> &[u16] {
    let start = u.iter().position(|&c| c > b' ' as u16).unwrap_or(u.len());
    let end = u.iter().rposition(|&c| c > b' ' as u16).map_or(start, |e| e + 1);
    &u[start..end]
}

/// `String.valueOf(Object)` for any value of descriptor `desc`.
pub fn value_of(vm: &mut Vm, v: Value, desc: &str) -> Res<Vec<u16>> {
    vm.stringify(v, desc)
}

static CLASSES: &[NativeClass] = &[
    NativeClass {
        name: "java/lang/String",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/io/Serializable", "java/lang/Comparable", "java/lang/CharSequence", "java/lang/constant/Constable", "java/lang/constant/ConstantDesc"],
        access: PUBLIC | FINAL,
        methods: STRING_METHODS,
        statics: &[("CASE_INSENSITIVE_ORDER", "Ljava/util/Comparator;")],
        clinit: Some(|vm, _| {
            let c = vm.load("java/lang/String")?;
            let cmp = super::util::native_comparator(vm, super::util::Cmp::CaseInsensitive)?;
            vm.set_static(c, "CASE_INSENSITIVE_ORDER", Value::Ref(cmp));
            void()
        }),
        data: Some(|| Data::Str(Box::new([]), 0)),
    },
    NativeClass {
        name: "java/lang/AbstractStringBuilder",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/lang/Appendable", "java/lang/CharSequence"],
        access: 0x0400,
        methods: &[],
        statics: &[],
        clinit: None,
        data: Some(|| Data::Chars(Vec::new())),
    },
    NativeClass {
        name: "java/lang/StringBuilder",
        super_name: Some("java/lang/AbstractStringBuilder"),
        interfaces: &["java/io/Serializable", "java/lang/Comparable", "java/lang/CharSequence"],
        access: PUBLIC | FINAL,
        methods: builder_methods!("Ljava/lang/StringBuilder;"),
        statics: &[],
        clinit: None,
        data: Some(|| Data::Chars(Vec::new())),
    },
    NativeClass {
        name: "java/lang/StringBuffer",
        super_name: Some("java/lang/AbstractStringBuilder"),
        interfaces: &["java/io/Serializable", "java/lang/Comparable", "java/lang/CharSequence"],
        access: PUBLIC | FINAL,
        methods: builder_methods!("Ljava/lang/StringBuffer;"),
        statics: &[],
        clinit: None,
        data: Some(|| Data::Chars(Vec::new())),
    },
    class("java/util/Formattable", None, &[], super::INTERFACE, &[]),
];

static STRING_METHODS: &[NativeMethod] = natives![
    "<init>" "()V" M => Some(|vm, a| set_this_str(vm, a, Vec::new())),
    "<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| { let u = units(vm, arg(a, 1).r())?; set_this_str(vm, a, u) }),
    "<init>" "([C)V" M => Some(|vm, a| { let u = chars_of(vm, arg(a, 1).r(), None)?; set_this_str(vm, a, u) }),
    "<init>" "([CII)V" M => Some(|vm, a| { let u = chars_of(vm, arg(a, 1).r(), Some((arg(a, 2).i(), arg(a, 3).i())))?; set_this_str(vm, a, u) }),
    "<init>" "([III)V" M => Some(|vm, a| { let cps = vm.array_values(arg(a, 1).r()); let (o, n) = (arg(a, 2).i() as usize, arg(a, 3).i() as usize); let u: Vec<u16> = cps.iter().skip(o).take(n).flat_map(|c| char_units(c.i())).collect(); set_this_str(vm, a, u) }),
    "<init>" "([B)V" M => Some(|vm, a| { let b = bytes_of(vm, arg(a, 1).r())?; set_this_str(vm, a, super::io::decode(&b, "UTF-8")) }),
    "<init>" "([BII)V" M => Some(|vm, a| { let b = bytes_of(vm, arg(a, 1).r())?; let s = slice(&b, arg(a, 2).i(), arg(a, 3).i()); set_this_str(vm, a, super::io::decode(s, "UTF-8")) }),
    "<init>" "([BLjava/nio/charset/Charset;)V" M => Some(|vm, a| { let b = bytes_of(vm, arg(a, 1).r())?; let cs = super::io::charset_name(vm, arg(a, 2).r()); set_this_str(vm, a, super::io::decode(&b, &cs)) }),
    "<init>" "([BIILjava/nio/charset/Charset;)V" M => Some(|vm, a| { let b = bytes_of(vm, arg(a, 1).r())?; let cs = super::io::charset_name(vm, arg(a, 4).r()); let s = slice(&b, arg(a, 2).i(), arg(a, 3).i()); set_this_str(vm, a, super::io::decode(s, &cs)) }),
    "<init>" "([BLjava/lang/String;)V" M => Some(|vm, a| { let b = bytes_of(vm, arg(a, 1).r())?; let cs = vm.string(arg(a, 2).r()).unwrap_or_default(); set_this_str(vm, a, super::io::decode(&b, &cs)) }),
    "<init>" "(Ljava/lang/StringBuilder;)V" M => Some(|vm, a| { let u = builder(vm, arg(a, 1).r())?; set_this_str(vm, a, u) }),
    "<init>" "(Ljava/lang/StringBuffer;)V" M => Some(|vm, a| { let u = builder(vm, arg(a, 1).r())?; set_this_str(vm, a, u) }),
    "length" "()I" M => Some(|vm, a| { let u = this_units(vm, a)?; Ok(Value::Int(u.len() as i32)) }),
    "isEmpty" "()Z" M => Some(|vm, a| { let u = this_units(vm, a)?; ret_bool(u.is_empty()) }),
    "isBlank" "()Z" M => Some(|vm, a| { let u = this_units(vm, a)?; ret_bool(u.iter().all(|&c| is_java_whitespace(c))) }),
    "charAt" "(I)C" M => Some(|vm, a| { let u = this_units(vm, a)?; let i = arg(a, 1).i(); match u.get(i as usize).filter(|_| i >= 0) { Some(&c) => Ok(Value::Int(c as i32)), None => { let msg = format!("Index {i} out of bounds for length {}", u.len()); Err(sioobe(vm, msg)) } } }),
    "codePointAt" "(I)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let i = arg(a, 1).i() as usize; if i >= u.len() { let msg = format!("Index {i} out of bounds for length {}", u.len()); return Err(sioobe(vm, msg)); } Ok(Value::Int(code_point_at(&u, i))) }),
    "codePointBefore" "(I)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let i = arg(a, 1).i() as usize; if i == 0 || i > u.len() { return Err(sioobe(vm, format!("index {i}"))); } let lo = u[i - 1]; Ok(Value::Int(if (0xDC00..0xE000).contains(&lo) && i >= 2 && (0xD800..0xDC00).contains(&u[i - 2]) { code_point_at(&u, i - 2) } else { lo as i32 })) }),
    "codePointCount" "(II)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let (b, e) = (arg(a, 1).i() as usize, arg(a, 2).i() as usize); let s = &u[b.min(u.len())..e.min(u.len())]; Ok(Value::Int(char::decode_utf16(s.iter().copied()).count() as i32)) }),
    "offsetByCodePoints" "(II)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let mut i = arg(a, 1).i() as usize; for _ in 0..arg(a, 2).i() { if i < u.len() { i += if (0xD800..0xDC00).contains(&u[i]) && i + 1 < u.len() { 2 } else { 1 }; } } Ok(Value::Int(i as i32)) }),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let o = arg(a, 1).r(); ret_bool(t == o || (vm.string_units(o).is_some() && vm.string_units(t) == vm.string_units(o))) }),
    "equalsIgnoreCase" "(Ljava/lang/String;)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let o = arg(a, 1).r(); if o == 0 { return ret_bool(false); } let v = units(vm, o)?; ret_bool(u.len() == v.len() && compare_ignore_case(&u, &v) == 0) }),
    "contentEquals" "(Ljava/lang/CharSequence;)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let v = units(vm, arg(a, 1).r())?; ret_bool(u == v) }),
    "hashCode" "()I" M => Some(|vm, a| { let t = this(vm, a)?; Ok(Value::Int(super::lang::object_hash(vm, t)?)) }),
    "compareTo" "(Ljava/lang/String;)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let v = units(vm, arg(a, 1).r())?; Ok(Value::Int(compare(&u, &v))) }),
    "compareTo" "(Ljava/lang/Object;)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let v = units(vm, arg(a, 1).r())?; Ok(Value::Int(compare(&u, &v))) }),
    "compareToIgnoreCase" "(Ljava/lang/String;)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let v = units(vm, arg(a, 1).r())?; Ok(Value::Int(compare_ignore_case(&u, &v))) }),
    "indexOf" "(I)I" M => Some(|vm, a| { let u = this_units(vm, a)?; Ok(Value::Int(index_of(&u, &char_units(arg(a, 1).i()), 0))) }),
    "indexOf" "(II)I" M => Some(|vm, a| { let u = this_units(vm, a)?; Ok(Value::Int(index_of(&u, &char_units(arg(a, 1).i()), arg(a, 2).i().max(0) as usize))) }),
    "indexOf" "(Ljava/lang/String;)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; Ok(Value::Int(index_of(&u, &n, 0))) }),
    "indexOf" "(Ljava/lang/String;I)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; Ok(Value::Int(index_of(&u, &n, arg(a, 2).i().max(0) as usize))) }),
    "lastIndexOf" "(I)I" M => Some(|vm, a| { let u = this_units(vm, a)?; Ok(Value::Int(last_index_of(&u, &char_units(arg(a, 1).i()), i32::MAX))) }),
    "lastIndexOf" "(II)I" M => Some(|vm, a| { let u = this_units(vm, a)?; Ok(Value::Int(last_index_of(&u, &char_units(arg(a, 1).i()), arg(a, 2).i()))) }),
    "lastIndexOf" "(Ljava/lang/String;)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; Ok(Value::Int(last_index_of(&u, &n, i32::MAX))) }),
    "lastIndexOf" "(Ljava/lang/String;I)I" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; Ok(Value::Int(last_index_of(&u, &n, arg(a, 2).i()))) }),
    "contains" "(Ljava/lang/CharSequence;)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; ret_bool(index_of(&u, &n, 0) >= 0) }),
    "startsWith" "(Ljava/lang/String;)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; ret_bool(u.starts_with(&n)) }),
    "startsWith" "(Ljava/lang/String;I)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; let o = arg(a, 2).i(); ret_bool(o >= 0 && (o as usize) <= u.len() && u[o as usize..].starts_with(&n)) }),
    "endsWith" "(Ljava/lang/String;)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = units(vm, arg(a, 1).r())?; ret_bool(u.ends_with(&n)) }),
    "regionMatches" "(ILjava/lang/String;II)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let o = units(vm, arg(a, 2).r())?; ret_bool(region(&u, arg(a, 1).i(), &o, arg(a, 3).i(), arg(a, 4).i(), false)) }),
    "regionMatches" "(ZILjava/lang/String;II)Z" M => Some(|vm, a| { let u = this_units(vm, a)?; let o = units(vm, arg(a, 3).r())?; ret_bool(region(&u, arg(a, 2).i(), &o, arg(a, 4).i(), arg(a, 5).i(), arg(a, 1).z())) }),
    "substring" "(I)Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let b = arg(a, 1).i(); substring(vm, &u, b, u.len() as i32) }),
    "substring" "(II)Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; substring(vm, &u, arg(a, 1).i(), arg(a, 2).i()) }),
    "subSequence" "(II)Ljava/lang/CharSequence;" M => Some(|vm, a| { let u = this_units(vm, a)?; substring(vm, &u, arg(a, 1).i(), arg(a, 2).i()) }),
    "toUpperCase" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; new_str(vm, case_map(&u, true)) }),
    "toUpperCase" "(Ljava/util/Locale;)Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; new_str(vm, case_map(&u, true)) }),
    "toLowerCase" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; new_str(vm, case_map(&u, false)) }),
    "toLowerCase" "(Ljava/util/Locale;)Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; new_str(vm, case_map(&u, false)) }),
    "trim" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let u = units(vm, t)?; let tr = java_trim(&u); if tr.len() == u.len() { return ret_ref(t); } new_str(vm, tr.to_vec()) }),
    "strip" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let s = u.iter().position(|&c| !is_java_whitespace(c)).unwrap_or(u.len()); let e = u.iter().rposition(|&c| !is_java_whitespace(c)).map_or(s, |e| e + 1); new_str(vm, u[s..e].to_vec()) }),
    "stripLeading" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let s = u.iter().position(|&c| !is_java_whitespace(c)).unwrap_or(u.len()); new_str(vm, u[s..].to_vec()) }),
    "stripTrailing" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let e = u.iter().rposition(|&c| !is_java_whitespace(c)).map_or(0, |e| e + 1); new_str(vm, u[..e].to_vec()) }),
    "replace" "(CC)Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let (f, t) = (arg(a, 1).i() as u16, arg(a, 2).i() as u16); new_str(vm, u.iter().map(|&c| if c == f { t } else { c }).collect()) }),
    "replace" "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;" M => Some(string_replace),
    "replaceAll" "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; super::util::regex_replace(vm, t, arg(a, 1).r(), arg(a, 2).r(), true) }),
    "replaceFirst" "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; super::util::regex_replace(vm, t, arg(a, 1).r(), arg(a, 2).r(), false) }),
    "matches" "(Ljava/lang/String;)Z" M => Some(|vm, a| { let t = this(vm, a)?; super::util::regex_matches(vm, arg(a, 1).r(), t) }),
    "split" "(Ljava/lang/String;)[Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; super::util::split(vm, t, arg(a, 1).r(), 0) }),
    "split" "(Ljava/lang/String;I)[Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; super::util::split(vm, t, arg(a, 1).r(), arg(a, 2).i()) }),
    "concat" "(Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| { let mut u = this_units(vm, a)?; let o = units(vm, arg(a, 1).r())?; if o.is_empty() { return Ok(arg(a, 0)); } u.extend(o); new_str(vm, u) }),
    "repeat" "(I)Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let n = arg(a, 1).i(); if n < 0 { let msg = format!("count is negative: {n}"); return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg))); } if u.len() * n as usize > vm.options.max_array { return Err(vm.out_of_memory("Required length exceeds implementation limit")); } new_str(vm, u.repeat(n as usize)) }),
    "toCharArray" "()[C" M => Some(|vm, a| { let u = this_units(vm, a)?; let r = vm.new_array_of("C", u.len() as i32)?; if let Some(Array::C(v)) = vm.array_mut(r) { v.copy_from_slice(&u); } ret_ref(r) }),
    "getChars" "(II[CI)V" M => Some(|vm, a| { let u = this_units(vm, a)?; let (b, e, dst, d) = (arg(a, 1).i() as usize, arg(a, 2).i() as usize, arg(a, 3).r(), arg(a, 4).i() as usize); if let Some(Array::C(v)) = vm.array_mut(dst) { if b <= e && e <= u.len() && d + (e - b) <= v.len() { v[d..d + e - b].copy_from_slice(&u[b..e]); } } void() }),
    "getBytes" "()[B" M => Some(|vm, a| { let u = this_units(vm, a)?; bytes_value(vm, super::io::encode(&u, "UTF-8")) }),
    "getBytes" "(Ljava/nio/charset/Charset;)[B" M => Some(|vm, a| { let u = this_units(vm, a)?; let cs = super::io::charset_name(vm, arg(a, 1).r()); bytes_value(vm, super::io::encode(&u, &cs)) }),
    "getBytes" "(Ljava/lang/String;)[B" M => Some(|vm, a| { let u = this_units(vm, a)?; let cs = vm.string(arg(a, 1).r()).unwrap_or_default(); bytes_value(vm, super::io::encode(&u, &cs)) }),
    "intern" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; ret_ref(vm.intern_units(&u)?) }),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; ret_ref(t) }),
    "chars" "()Ljava/util/stream/IntStream;" M => Some(|vm, a| { let u = this_units(vm, a)?; super::stream::int_stream_of(vm, u.iter().map(|&c| Value::Int(c as i32)).collect()) }),
    "codePoints" "()Ljava/util/stream/IntStream;" M => Some(|vm, a| { let u = this_units(vm, a)?; let cps: Vec<Value> = char::decode_utf16(u.iter().copied()).map(|c| Value::Int(match c { Ok(c) => c as i32, Err(e) => e.unpaired_surrogate() as i32 })).collect(); super::stream::int_stream_of(vm, cps) }),
    "lines" "()Ljava/util/stream/Stream;" M => Some(|vm, a| { let u = this_units(vm, a)?; let mut items = Vec::new(); for line in split_lines(&u) { items.push(Value::Ref(vm.new_string_units(line)?)); } super::stream::stream_of(vm, items) }),
    "indent" "(I)Ljava/lang/String;" M => Some(string_indent),
    "stripIndent" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; ret_ref(t) }),
    "translateEscapes" "()Ljava/lang/String;" M => Some(|vm, a| { let u = this_units(vm, a)?; let out = translate_escapes(&u).map_err(|m| vm.throw_new("java/lang/IllegalArgumentException", Some(&m)))?; new_str(vm, out) }),
    "formatted" "([Ljava/lang/Object;)Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let args = vm.ref_elements(arg(a, 1).r()); let s = format(vm, t, &args)?; new_str(vm, s) }),
    "describeConstable" "()Ljava/util/Optional;" M => Some(|vm, a| super::util::optional_of(vm, arg(a, 0))),
    "valueOf" "(Ljava/lang/Object;)Ljava/lang/String;" S => Some(|vm, a| { let u = value_of(vm, arg(a, 0), "Ljava/lang/Object;")?; if let Some(_) = vm.string_units(arg(a, 0).r()) { return Ok(arg(a, 0)); } new_str(vm, u) }),
    "valueOf" "(I)Ljava/lang/String;" S => Some(|vm, a| { let u = value_of(vm, arg(a, 0), "I")?; new_str(vm, u) }),
    "valueOf" "(J)Ljava/lang/String;" S => Some(|vm, a| { let u = value_of(vm, arg(a, 0), "J")?; new_str(vm, u) }),
    "valueOf" "(F)Ljava/lang/String;" S => Some(|vm, a| { let u = value_of(vm, arg(a, 0), "F")?; new_str(vm, u) }),
    "valueOf" "(D)Ljava/lang/String;" S => Some(|vm, a| { let u = value_of(vm, arg(a, 0), "D")?; new_str(vm, u) }),
    "valueOf" "(Z)Ljava/lang/String;" S => Some(|vm, a| { let u = value_of(vm, arg(a, 0), "Z")?; new_str(vm, u) }),
    "valueOf" "(C)Ljava/lang/String;" S => Some(|vm, a| new_str(vm, vec![arg(a, 0).i() as u16])),
    "valueOf" "([C)Ljava/lang/String;" S => Some(|vm, a| { let u = chars_of(vm, arg(a, 0).r(), None)?; new_str(vm, u) }),
    "valueOf" "([CII)Ljava/lang/String;" S => Some(|vm, a| { let u = chars_of(vm, arg(a, 0).r(), Some((arg(a, 1).i(), arg(a, 2).i())))?; new_str(vm, u) }),
    "copyValueOf" "([C)Ljava/lang/String;" S => Some(|vm, a| { let u = chars_of(vm, arg(a, 0).r(), None)?; new_str(vm, u) }),
    "copyValueOf" "([CII)Ljava/lang/String;" S => Some(|vm, a| { let u = chars_of(vm, arg(a, 0).r(), Some((arg(a, 1).i(), arg(a, 2).i())))?; new_str(vm, u) }),
    "format" "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;" S => Some(|vm, a| { let args = vm.ref_elements(arg(a, 1).r()); let s = format(vm, arg(a, 0).r(), &args)?; new_str(vm, s) }),
    "format" "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;" S => Some(|vm, a| { let args = vm.ref_elements(arg(a, 2).r()); let s = format(vm, arg(a, 1).r(), &args)?; new_str(vm, s) }),
    "join" "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;" S => Some(|vm, a| { let sep = units(vm, arg(a, 0).r())?; let items = vm.ref_elements(arg(a, 1).r()); join(vm, &sep, items) }),
    "join" "(Ljava/lang/CharSequence;Ljava/lang/Iterable;)Ljava/lang/String;" S => Some(|vm, a| { let sep = units(vm, arg(a, 0).r())?; let items = super::util::iterate(vm, arg(a, 1).r())?; join(vm, &sep, items.iter().map(|v| v.r()).collect()) }),
];

fn set_this_str(vm: &mut Vm, a: &[Value], u: Vec<u16>) -> Res<Value> {
    let t = this(vm, a)?;
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Str(u.into_boxed_slice(), 0);
    }
    void()
}

fn code_point_at(u: &[u16], i: usize) -> i32 {
    let hi = u[i];
    if (0xD800..0xDC00).contains(&hi)
        && let Some(&lo) = u.get(i + 1)
        && (0xDC00..0xE000).contains(&lo)
    {
        return 0x10000 + (((hi as i32) - 0xD800) << 10) + (lo as i32 - 0xDC00);
    }
    hi as i32
}

fn region(u: &[u16], to: i32, o: &[u16], oo: i32, len: i32, ignore: bool) -> bool {
    if to < 0 || oo < 0 || to as i64 + len as i64 > u.len() as i64 || oo as i64 + len as i64 > o.len() as i64 {
        return false;
    }
    if len <= 0 {
        return true;
    }
    let (a, b) = (&u[to as usize..(to + len) as usize], &o[oo as usize..(oo + len) as usize]);
    if ignore { compare_ignore_case(a, b) == 0 } else { a == b }
}

fn substring(vm: &mut Vm, u: &[u16], b: i32, e: i32) -> Res<Value> {
    if b < 0 || e > u.len() as i32 || b > e {
        let msg = format!("begin {b}, end {e}, length {}", u.len());
        return Err(sioobe(vm, msg));
    }
    new_str(vm, u[b as usize..e as usize].to_vec())
}

fn string_replace(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let u = this_units(vm, a)?;
    let from = units(vm, arg(a, 1).r())?;
    let to = units(vm, arg(a, 2).r())?;
    let mut out = Vec::with_capacity(u.len());
    if from.is_empty() {
        for &c in &u {
            out.extend_from_slice(&to);
            out.push(c);
        }
        out.extend_from_slice(&to);
        return new_str(vm, out);
    }
    let mut i = 0;
    while i < u.len() {
        if u[i..].starts_with(&from) {
            out.extend_from_slice(&to);
            i += from.len();
        } else {
            out.push(u[i]);
            i += 1;
        }
    }
    new_str(vm, out)
}

fn split_lines(u: &[u16]) -> Vec<Vec<u16>> {
    let mut lines = Vec::new();
    let mut cur = Vec::new();
    let mut i = 0;
    while i < u.len() {
        match u[i] {
            10 => lines.push(std::mem::take(&mut cur)),
            13 => {
                lines.push(std::mem::take(&mut cur));
                if u.get(i + 1) == Some(&10) {
                    i += 1;
                }
            }
            c => cur.push(c),
        }
        i += 1;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn string_indent(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let u = this_units(vm, a)?;
    let n = arg(a, 1).i();
    if u.is_empty() {
        return new_str(vm, u);
    }
    let mut out = Vec::new();
    for line in split_lines(&u) {
        if n > 0 {
            out.extend(std::iter::repeat_n(b' ' as u16, n as usize));
            out.extend(line);
        } else if n < 0 {
            let skip = line.iter().take((-n) as usize).take_while(|&&c| is_java_whitespace(c)).count();
            out.extend_from_slice(&line[skip..]);
        } else {
            out.extend(line);
        }
        out.push(10);
    }
    new_str(vm, out)
}

fn translate_escapes(u: &[u16]) -> Result<Vec<u16>, String> {
    let mut out = Vec::with_capacity(u.len());
    let mut i = 0;
    while i < u.len() {
        let c = u[i];
        i += 1;
        if c != b'\\' as u16 {
            out.push(c);
            continue;
        }
        let Some(&e) = u.get(i) else {
            return Err("Invalid escape sequence: \\".into());
        };
        i += 1;
        let mapped = match e as u8 {
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'f' => 12,
            b'r' => 13,
            b's' => 32,
            b'"' => 34,
            b'\'' => 39,
            b'\\' => 92,
            b'\n' => continue,
            b'0'..=b'7' => {
                let mut v = (e - b'0' as u16) as u32;
                let limit = if e <= b'3' as u16 { 2 } else { 1 };
                for _ in 0..limit {
                    match u.get(i) {
                        Some(&d) if (b'0' as u16..=b'7' as u16).contains(&d) => {
                            v = v * 8 + (d - b'0' as u16) as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                v as u16
            }
            other => return Err(format!("Invalid escape sequence: \\{} \\\\u{:04X}", other as char, other)),
        };
        out.push(mapped);
    }
    Ok(out)
}

fn join(vm: &mut Vm, sep: &[u16], items: Vec<ObjId>) -> Res<Value> {
    let mut out = Vec::new();
    for (i, r) in items.into_iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        let s = vm.stringify(Value::Ref(r), "Ljava/lang/Object;")?;
        out.extend(s);
    }
    new_str(vm, out)
}

/// A `char[]` argument's elements (a range of them).
pub fn chars_of(vm: &mut Vm, r: ObjId, range: Option<(i32, i32)>) -> Res<Vec<u16>> {
    let v = match vm.array(r) {
        Some(Array::C(v)) => v.clone(),
        _ if r == 0 => return Err(vm.npe(None)),
        _ => return Err(Exc::Internal("expected a char[]".into())),
    };
    match range {
        None => Ok(v),
        Some((o, n)) => {
            if o < 0 || n < 0 || o as usize + n as usize > v.len() {
                let msg = format!("offset {o}, count {n}, length {}", v.len());
                return Err(sioobe(vm, msg));
            }
            Ok(v[o as usize..(o + n) as usize].to_vec())
        }
    }
}

/// A `byte[]` argument's elements.
pub fn bytes_of(vm: &mut Vm, r: ObjId) -> Res<Vec<u8>> {
    match vm.array(r) {
        Some(Array::B(v)) => Ok(v.iter().map(|&b| b as u8).collect()),
        _ if r == 0 => Err(vm.npe(None)),
        _ => Err(Exc::Internal("expected a byte[]".into())),
    }
}

fn slice(b: &[u8], o: i32, n: i32) -> &[u8] {
    let o = (o.max(0) as usize).min(b.len());
    let e = (o + n.max(0) as usize).min(b.len());
    &b[o..e]
}

pub fn bytes_value(vm: &mut Vm, b: Vec<u8>) -> Res<Value> {
    let r = vm.new_array_of("B", b.len() as i32)?;
    if let Some(Array::B(v)) = vm.array_mut(r) {
        for (d, s) in v.iter_mut().zip(b) {
            *d = s as i8;
        }
    }
    ret_ref(r)
}

pub fn char_sequence_chars(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let u = units(vm, t)?;
    super::stream::int_stream_of(vm, u.iter().map(|&c| Value::Int(c as i32)).collect())
}

pub fn char_sequence_compare(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let x = units(vm, arg(a, 0).r())?;
    let y = units(vm, arg(a, 1).r())?;
    Ok(Value::Int(compare(&x, &y)))
}

// ---------------------------------------------------------------- builders

/// A builder's characters (StringBuilder or StringBuffer).
pub fn builder(vm: &mut Vm, r: ObjId) -> Res<Vec<u16>> {
    match vm.obj(r).map(|o| &o.data) {
        Some(Data::Chars(c)) => Ok(c.clone()),
        _ if r == 0 => Err(vm.npe(None)),
        _ => units(vm, r),
    }
}

fn chars_mut(vm: &mut Vm, r: ObjId) -> Option<&mut Vec<u16>> {
    match vm.obj_mut(r).map(|o| &mut o.data) {
        Some(Data::Chars(c)) => Some(c),
        _ => None,
    }
}

fn sb_append(vm: &mut Vm, a: &[Value], desc: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let text = match desc {
        "[C" => chars_of(vm, arg(a, 1).r(), None)?,
        "[CII" => chars_of(vm, arg(a, 1).r(), Some((arg(a, 2).i(), arg(a, 3).i())))?,
        "Ljava/lang/CharSequence;II" => {
            let r = arg(a, 1).r();
            let u = if r == 0 { "null".encode_utf16().collect() } else { units(vm, r)? };
            let (s, e) = (arg(a, 2).i(), arg(a, 3).i());
            if s < 0 || e < s || e as usize > u.len() {
                let msg = format!("start {s}, end {e}, length {}", u.len());
                return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
            }
            u[s as usize..e as usize].to_vec()
        }
        "codepoint" => char_units(arg(a, 1).i()),
        d => {
            let v = arg(a, 1);
            match vm.obj(v.r()).map(|o| &o.data) {
                Some(Data::Chars(c)) if d.starts_with('L') => c.clone(),
                _ => vm.stringify(v, d)?,
            }
        }
    };
    if let Some(c) = chars_mut(vm, t) {
        c.extend_from_slice(&text);
    }
    ret_ref(t)
}

fn sb_insert(vm: &mut Vm, a: &[Value], desc: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let at = arg(a, 1).i();
    let text = match desc {
        "[C" => chars_of(vm, arg(a, 2).r(), None)?,
        d => vm.stringify(arg(a, 2), d)?,
    };
    let len = chars_mut(vm, t).map_or(0, |c| c.len());
    if at < 0 || at as usize > len {
        let msg = format!("offset {at}, length {len}");
        return Err(sioobe(vm, msg));
    }
    if let Some(c) = chars_mut(vm, t) {
        c.splice(at as usize..at as usize, text);
    }
    ret_ref(t)
}

fn sb_check(vm: &mut Vm, t: ObjId, start: i32, end: i32, delete: bool) -> Res<(usize, usize)> {
    let len = chars_mut(vm, t).map_or(0, |c| c.len()) as i32;
    let end = if delete { end.min(len) } else { end };
    if start < 0 || start > end || end > len {
        let msg = format!("start {start}, end {end}, length {len}");
        return Err(sioobe(vm, msg));
    }
    Ok((start as usize, end as usize))
}

fn sb_methods_impl(vm: &mut Vm, a: &[Value], which: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match which {
        "init" => {
            let init: Vec<u16> = match a.get(1) {
                Some(Value::Ref(r)) => {
                    if *r == 0 {
                        return Err(vm.npe(None));
                    }
                    builder(vm, *r)?
                }
                Some(Value::Int(n)) if *n < 0 => {
                    return Err(vm.throw_new("java/lang/NegativeArraySizeException", Some(&n.to_string())));
                }
                _ => Vec::new(),
            };
            if let Some(o) = vm.obj_mut(t) {
                o.data = Data::Chars(init);
            }
            void()
        }
        "length" => Ok(Value::Int(builder(vm, t)?.len() as i32)),
        "isEmpty" => ret_bool(builder(vm, t)?.is_empty()),
        "toString" => {
            let c = builder(vm, t)?;
            new_str(vm, c)
        }
        "reverse" => {
            if let Some(c) = chars_mut(vm, t) {
                // Surrogate pairs stay in order, as Java's reverse keeps them.
                let s: Vec<u16> = {
                    let decoded: Vec<_> = char::decode_utf16(c.iter().copied()).collect();
                    decoded
                        .into_iter()
                        .rev()
                        .flat_map(|r| match r {
                            Ok(ch) => {
                                let mut b = [0u16; 2];
                                ch.encode_utf16(&mut b).to_vec()
                            }
                            Err(e) => vec![e.unpaired_surrogate()],
                        })
                        .collect()
                };
                *c = s;
            }
            ret_ref(t)
        }
        "charAt" => {
            let i = arg(a, 1).i();
            let c = builder(vm, t)?;
            match c.get(i as usize).filter(|_| i >= 0) {
                Some(&ch) => Ok(Value::Int(ch as i32)),
                None => {
                    let msg = format!("index {i},length {}", c.len());
                    Err(sioobe(vm, msg))
                }
            }
        }
        "setCharAt" => {
            let (i, ch) = (arg(a, 1).i(), arg(a, 2).i() as u16);
            let len = builder(vm, t)?.len();
            if i < 0 || i as usize >= len {
                let msg = format!("index {i},length {len}");
                return Err(sioobe(vm, msg));
            }
            if let Some(c) = chars_mut(vm, t) {
                c[i as usize] = ch;
            }
            void()
        }
        "deleteCharAt" => {
            let i = arg(a, 1).i();
            let len = builder(vm, t)?.len();
            if i < 0 || i as usize >= len {
                let msg = format!("index {i},length {len}");
                return Err(sioobe(vm, msg));
            }
            if let Some(c) = chars_mut(vm, t) {
                c.remove(i as usize);
            }
            ret_ref(t)
        }
        "delete" => {
            let (s, e) = sb_check(vm, t, arg(a, 1).i(), arg(a, 2).i(), true)?;
            if let Some(c) = chars_mut(vm, t) {
                c.drain(s..e);
            }
            ret_ref(t)
        }
        "replace" => {
            let (s, e) = sb_check(vm, t, arg(a, 1).i(), arg(a, 2).i(), true)?;
            let with = units(vm, arg(a, 3).r())?;
            if let Some(c) = chars_mut(vm, t) {
                c.splice(s..e, with);
            }
            ret_ref(t)
        }
        "setLength" => {
            let n = arg(a, 1).i();
            if n < 0 {
                return Err(sioobe(vm, format!("String index out of range: {n}")));
            }
            if let Some(c) = chars_mut(vm, t) {
                c.resize(n as usize, 0);
            }
            void()
        }
        "indexOf" | "indexOfFrom" | "lastIndexOf" | "lastIndexOfFrom" => {
            let c = builder(vm, t)?;
            let n = units(vm, arg(a, 1).r())?;
            Ok(Value::Int(match which {
                "indexOf" => index_of(&c, &n, 0),
                "indexOfFrom" => index_of(&c, &n, arg(a, 2).i().max(0) as usize),
                "lastIndexOf" => last_index_of(&c, &n, i32::MAX),
                _ => last_index_of(&c, &n, arg(a, 2).i()),
            }))
        }
        "substring" | "substring2" | "subSequence" => {
            let c = builder(vm, t)?;
            let e = if which == "substring" { c.len() as i32 } else { arg(a, 2).i() };
            substring(vm, &c, arg(a, 1).i(), e)
        }
        "capacity" => Ok(Value::Int(builder(vm, t)?.len().max(16) as i32)),
        "compareTo" => {
            let (x, y) = (builder(vm, t)?, builder(vm, arg(a, 1).r())?);
            Ok(Value::Int(compare(&x, &y)))
        }
        "chars" => {
            let c = builder(vm, t)?;
            super::stream::int_stream_of(vm, c.iter().map(|&u| Value::Int(u as i32)).collect())
        }
        "codePointAt" => {
            let c = builder(vm, t)?;
            let i = arg(a, 1).i() as usize;
            if i >= c.len() {
                return Err(sioobe(vm, format!("index {i}")));
            }
            Ok(Value::Int(code_point_at(&c, i)))
        }
        "getChars" => {
            let c = builder(vm, t)?;
            let (b, e, dst, d) = (arg(a, 1).i() as usize, arg(a, 2).i() as usize, arg(a, 3).r(), arg(a, 4).i() as usize);
            if let Some(Array::C(v)) = vm.array_mut(dst)
                && b <= e
                && e <= c.len()
                && d + (e - b) <= v.len()
            {
                v[d..d + e - b].copy_from_slice(&c[b..e]);
            }
            void()
        }
        _ => void(),
    }
}

/// The methods of a builder class whose fluent methods return `$ret`.
macro_rules! builder_methods {
    ($ret:literal) => {
        natives![
            "<init>" "()V" M => Some(|vm, a| sb_methods_impl(vm, a, "init")),
            "<init>" "(I)V" M => Some(|vm, a| sb_methods_impl(vm, a, "init")),
            "<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| sb_methods_impl(vm, a, "init")),
            "<init>" "(Ljava/lang/CharSequence;)V" M => Some(|vm, a| sb_methods_impl(vm, a, "init")),
            "append" (concat!("(Ljava/lang/String;)", $ret)) M => Some(|vm, a| sb_append(vm, a, "Ljava/lang/String;")),
            "append" (concat!("(Ljava/lang/Object;)", $ret)) M => Some(|vm, a| sb_append(vm, a, "Ljava/lang/Object;")),
            "append" (concat!("(Ljava/lang/CharSequence;)", $ret)) M => Some(|vm, a| sb_append(vm, a, "Ljava/lang/Object;")),
            "append" (concat!("(Ljava/lang/CharSequence;II)", $ret)) M => Some(|vm, a| sb_append(vm, a, "Ljava/lang/CharSequence;II")),
            "append" (concat!("(Ljava/lang/StringBuffer;)", $ret)) M => Some(|vm, a| sb_append(vm, a, "Ljava/lang/Object;")),
            "append" (concat!("(I)", $ret)) M => Some(|vm, a| sb_append(vm, a, "I")),
            "append" (concat!("(J)", $ret)) M => Some(|vm, a| sb_append(vm, a, "J")),
            "append" (concat!("(F)", $ret)) M => Some(|vm, a| sb_append(vm, a, "F")),
            "append" (concat!("(D)", $ret)) M => Some(|vm, a| sb_append(vm, a, "D")),
            "append" (concat!("(Z)", $ret)) M => Some(|vm, a| sb_append(vm, a, "Z")),
            "append" (concat!("(C)", $ret)) M => Some(|vm, a| sb_append(vm, a, "C")),
            "append" (concat!("([C)", $ret)) M => Some(|vm, a| sb_append(vm, a, "[C")),
            "append" (concat!("([CII)", $ret)) M => Some(|vm, a| sb_append(vm, a, "[CII")),
            "append" "(C)Ljava/lang/Appendable;" M => Some(|vm, a| sb_append(vm, a, "C")),
            "append" "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;" M => Some(|vm, a| sb_append(vm, a, "Ljava/lang/Object;")),
            "appendCodePoint" (concat!("(I)", $ret)) M => Some(|vm, a| sb_append(vm, a, "codepoint")),
            "insert" (concat!("(ILjava/lang/String;)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "Ljava/lang/String;")),
            "insert" (concat!("(ILjava/lang/Object;)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "Ljava/lang/Object;")),
            "insert" (concat!("(ILjava/lang/CharSequence;)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "Ljava/lang/Object;")),
            "insert" (concat!("(II)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "I")),
            "insert" (concat!("(IJ)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "J")),
            "insert" (concat!("(IF)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "F")),
            "insert" (concat!("(ID)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "D")),
            "insert" (concat!("(IZ)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "Z")),
            "insert" (concat!("(IC)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "C")),
            "insert" (concat!("(I[C)", $ret)) M => Some(|vm, a| sb_insert(vm, a, "[C")),
            "reverse" (concat!("()", $ret)) M => Some(|vm, a| sb_methods_impl(vm, a, "reverse")),
            "delete" (concat!("(II)", $ret)) M => Some(|vm, a| sb_methods_impl(vm, a, "delete")),
            "deleteCharAt" (concat!("(I)", $ret)) M => Some(|vm, a| sb_methods_impl(vm, a, "deleteCharAt")),
            "replace" (concat!("(IILjava/lang/String;)", $ret)) M => Some(|vm, a| sb_methods_impl(vm, a, "replace")),
            "length" "()I" M => Some(|vm, a| sb_methods_impl(vm, a, "length")),
            "isEmpty" "()Z" M => Some(|vm, a| sb_methods_impl(vm, a, "isEmpty")),
            "capacity" "()I" M => Some(|vm, a| sb_methods_impl(vm, a, "capacity")),
            "ensureCapacity" "(I)V" M => Some(|_, _| void()),
            "trimToSize" "()V" M => Some(|_, _| void()),
            "charAt" "(I)C" M => Some(|vm, a| sb_methods_impl(vm, a, "charAt")),
            "codePointAt" "(I)I" M => Some(|vm, a| sb_methods_impl(vm, a, "codePointAt")),
            "setCharAt" "(IC)V" M => Some(|vm, a| sb_methods_impl(vm, a, "setCharAt")),
            "setLength" "(I)V" M => Some(|vm, a| sb_methods_impl(vm, a, "setLength")),
            "indexOf" "(Ljava/lang/String;)I" M => Some(|vm, a| sb_methods_impl(vm, a, "indexOf")),
            "indexOf" "(Ljava/lang/String;I)I" M => Some(|vm, a| sb_methods_impl(vm, a, "indexOfFrom")),
            "lastIndexOf" "(Ljava/lang/String;)I" M => Some(|vm, a| sb_methods_impl(vm, a, "lastIndexOf")),
            "lastIndexOf" "(Ljava/lang/String;I)I" M => Some(|vm, a| sb_methods_impl(vm, a, "lastIndexOfFrom")),
            "substring" "(I)Ljava/lang/String;" M => Some(|vm, a| sb_methods_impl(vm, a, "substring")),
            "substring" "(II)Ljava/lang/String;" M => Some(|vm, a| sb_methods_impl(vm, a, "substring2")),
            "subSequence" "(II)Ljava/lang/CharSequence;" M => Some(|vm, a| sb_methods_impl(vm, a, "subSequence")),
            "getChars" "(II[CI)V" M => Some(|vm, a| sb_methods_impl(vm, a, "getChars")),
            "toString" "()Ljava/lang/String;" M => Some(|vm, a| sb_methods_impl(vm, a, "toString")),
            "compareTo" (concat!("(", $ret, ")I")) M => Some(|vm, a| sb_methods_impl(vm, a, "compareTo")),
            "compareTo" "(Ljava/lang/Object;)I" M => Some(|vm, a| sb_methods_impl(vm, a, "compareTo")),
            "chars" "()Ljava/util/stream/IntStream;" M => Some(|vm, a| sb_methods_impl(vm, a, "chars")),
        ]
    };
}
use builder_methods;

// ---------------------------------------------------------------- format

/// `String.format` and `Formatter.format` in the `en` locale.
pub fn format(vm: &mut Vm, fmt: ObjId, args: &[ObjId]) -> Res<Vec<u16>> {
    let f = units(vm, fmt)?;
    let f = String::from_utf16_lossy(&f);
    let mut out = String::new();
    let mut ordinary = 0usize;
    let mut last: Option<usize> = None;
    let chars: Vec<char> = f.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        // %[argument_index$][flags][width][.precision]conversion
        let mut index: Option<usize> = None;
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j < chars.len() && chars[j] == '$' && j > i {
            index = chars[i..j].iter().collect::<String>().parse::<usize>().ok();
            i = j + 1;
        }
        let mut flags = String::new();
        while i < chars.len() && "-#+ 0,(<".contains(chars[i]) {
            flags.push(chars[i]);
            i += 1;
        }
        let mut width: Option<usize> = None;
        let ws = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i > ws {
            width = chars[ws..i].iter().collect::<String>().parse().ok();
        }
        let mut precision: Option<usize> = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let ps = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            precision = chars[ps..i].iter().collect::<String>().parse().ok();
        }
        let Some(&conv) = chars.get(i) else {
            let msg = format!("Format specifier '{}'", chars[start..].iter().collect::<String>());
            return Err(vm.throw_new("java/util/UnknownFormatConversionException", Some(&msg)));
        };
        i += 1;
        let (conv, upper) = match conv {
            'S' | 'X' | 'E' | 'G' | 'B' | 'H' | 'A' | 'C' => (conv.to_ascii_lowercase(), true),
            'T' => ('t', true),
            c => (c, false),
        };
        if conv == 'n' {
            out.push('\n');
            continue;
        }
        if conv == '%' {
            let s = pad("%".to_string(), width, flags.contains('-'));
            out.push_str(&s);
            continue;
        }
        let arg_index = if flags.contains('<') {
            last
        } else if let Some(n) = index {
            Some(n.saturating_sub(1))
        } else {
            ordinary += 1;
            Some(ordinary - 1)
        };
        last = arg_index;
        let Some(a) = arg_index.and_then(|n| args.get(n).copied()) else {
            let spec: String = chars[start..i].iter().collect();
            let msg = format!("Format specifier '{spec}'");
            return Err(vm.throw_new("java/util/MissingFormatArgumentException", Some(&msg)));
        };
        let left = flags.contains('-');
        let text = match conv {
            'b' => {
                let b = if a == 0 {
                    "false".to_string()
                } else {
                    match vm.obj(a).map(|o| &o.data) {
                        Some(Data::Boxed(Value::Int(v))) if vm.class_name(vm.class_of(a).unwrap()) == "java/lang/Boolean" => (*v != 0).to_string(),
                        _ => "true".to_string(),
                    }
                };
                truncate(b, precision)
            }
            's' => {
                let s = String::from_utf16_lossy(&vm.stringify(Value::Ref(a), "Ljava/lang/Object;")?);
                truncate(s, precision)
            }
            'h' => {
                if a == 0 {
                    "null".to_string()
                } else {
                    format!("{:x}", super::lang::object_hash(vm, a)? as u32)
                }
            }
            'c' => {
                if a == 0 {
                    "null".to_string()
                } else {
                    match vm.obj(a).map(|o| &o.data) {
                        Some(Data::Boxed(v)) => char::from_u32(v.i() as u32).map(String::from).unwrap_or_default(),
                        _ => return Err(conversion_error(vm, conv, a)),
                    }
                }
            }
            'd' | 'o' | 'x' => {
                if a == 0 {
                    "null".to_string()
                } else {
                    format_integer(vm, a, conv, &flags, width)?
                }
            }
            'f' | 'e' | 'g' | 'a' => {
                if a == 0 {
                    "null".to_string()
                } else {
                    format_float(vm, a, conv, &flags, width, precision)?
                }
            }
            't' => return Err(Exc::Unsupported("String.format %t".into())),
            other => {
                let msg = format!("Conversion = '{other}'");
                return Err(vm.throw_new("java/util/UnknownFormatConversionException", Some(&msg)));
            }
        };
        let text = if upper { text.to_uppercase() } else { text };
        out.push_str(&pad(text, width, left));
    }
    Ok(out.encode_utf16().collect())
}

fn conversion_error(vm: &mut Vm, conv: char, a: ObjId) -> Exc {
    let name = vm.class_of(a).map(|c| vm.class(c).java_name()).unwrap_or_default();
    let msg = format!("{conv} != {name}");
    vm.throw_new("java/util/IllegalFormatConversionException", Some(&msg))
}

fn truncate(s: String, precision: Option<usize>) -> String {
    match precision {
        Some(p) => s.chars().take(p).collect(),
        None => s,
    }
}

fn pad(s: String, width: Option<usize>, left: bool) -> String {
    let n = s.chars().count();
    match width {
        Some(w) if w > n => {
            let fill = " ".repeat(w - n);
            if left { s + &fill } else { fill + &s }
        }
        _ => s,
    }
}

fn group(digits: &str) -> String {
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Applies the sign flags (`+`, ` `, `(`), zero padding and grouping to a
/// number's magnitude.
fn signed(negative: bool, magnitude: String, flags: &str, width: Option<usize>, grouping_ok: bool) -> String {
    let mut digits = magnitude;
    if grouping_ok && flags.contains(',') {
        let (int, frac) = match digits.find('.') {
            Some(p) => (digits[..p].to_string(), digits[p..].to_string()),
            None => (digits.clone(), String::new()),
        };
        digits = group(&int) + &frac;
    }
    let (prefix, suffix) = if negative {
        if flags.contains('(') { ("(", ")") } else { ("-", "") }
    } else if flags.contains('+') {
        ("+", "")
    } else if flags.contains(' ') {
        (" ", "")
    } else {
        ("", "")
    };
    if flags.contains('0')
        && let Some(w) = width
    {
        let used = prefix.len() + suffix.len() + digits.len();
        if w > used {
            digits = "0".repeat(w - used) + &digits;
        }
    }
    format!("{prefix}{digits}{suffix}")
}

fn format_integer(vm: &mut Vm, a: ObjId, conv: char, flags: &str, width: Option<usize>) -> Res<String> {
    let class = vm.class_of(a).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
    if class == "java/math/BigInteger" {
        let big = super::math::big_integer_value(vm, a);
        let text = match conv {
            'd' => signed(big.sign < 0, big.magnitude_string(10), flags, width, true),
            'o' => signed(big.sign < 0, big.magnitude_string(8), flags, width, false),
            _ => signed(big.sign < 0, big.magnitude_string(16), flags, width, false),
        };
        return Ok(text);
    }
    let (value, bits) = match (vm.obj(a).map(|o| &o.data), class.as_str()) {
        (Some(Data::Boxed(Value::Long(v))), _) => (*v, 64),
        (Some(Data::Boxed(Value::Int(v))), "java/lang/Byte") => (*v as i64, 8),
        (Some(Data::Boxed(Value::Int(v))), "java/lang/Short") => (*v as i64, 16),
        (Some(Data::Boxed(Value::Int(v))), "java/lang/Integer") => (*v as i64, 32),
        _ => return Err(conversion_error(vm, conv, a)),
    };
    Ok(match conv {
        'd' => signed(value < 0, value.unsigned_abs().to_string(), flags, width, true),
        _ => {
            let unsigned = if bits == 64 { value as u64 } else { (value as u64) & ((1u64 << bits) - 1) };
            let mut s = if conv == 'o' { format!("{unsigned:o}") } else { format!("{unsigned:x}") };
            if flags.contains('#') {
                s = if conv == 'o' { format!("0{s}") } else { format!("0x{s}") };
            }
            if flags.contains('0')
                && let Some(w) = width
                && w > s.len()
            {
                s = "0".repeat(w - s.len()) + &s;
            }
            s
        }
    })
}

fn format_float(vm: &mut Vm, a: ObjId, conv: char, flags: &str, width: Option<usize>, precision: Option<usize>) -> Res<String> {
    let class = vm.class_of(a).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
    let (value, is_float) = match vm.obj(a).map(|o| &o.data) {
        Some(Data::Boxed(Value::Double(v))) => (*v, false),
        Some(Data::Boxed(Value::Float(v))) => (*v as f64, true),
        _ if class == "java/math/BigDecimal" => {
            let text = super::math::big_decimal_plain(vm, a);
            let v: f64 = text.parse().unwrap_or(0.0);
            (v, false)
        }
        _ => return Err(conversion_error(vm, conv, a)),
    };
    if value.is_nan() || value.is_infinite() {
        let s = if value.is_nan() {
            "NaN".to_string()
        } else if value > 0.0 {
            if flags.contains('+') { "+Infinity".into() } else { "Infinity".into() }
        } else if flags.contains('(') {
            "(Infinity)".into()
        } else {
            "-Infinity".into()
        };
        return Ok(s);
    }
    let negative = value.is_sign_negative() && value != 0.0 || (value == 0.0 && value.is_sign_negative());
    let digits = if is_float {
        shortest_digits_f32(value.abs() as f32)
    } else {
        shortest_digits(value.abs())
    };
    Ok(match conv {
        'f' => {
            let p = precision.unwrap_or(6);
            signed(negative, fixed(&digits, p), flags, width, true)
        }
        'e' => {
            let p = precision.unwrap_or(6);
            signed(negative, scientific(&digits, p), flags, width, false)
        }
        'g' => {
            let p = match precision {
                Some(0) => 1,
                Some(p) => p,
                None => 6,
            };
            // Rounded to p significant digits, then laid out by its exponent.
            let rounded = round_digits(&digits, p);
            let exp = rounded.1;
            let text = if value != 0.0 && (exp < -4 || exp >= p as i32) {
                scientific(&digits, p - 1)
            } else {
                fixed(&digits, (p as i32 - 1 - exp).max(0) as usize)
            };
            signed(negative, text, flags, width, true)
        }
        _ => {
            let bits = value.abs().to_bits();
            let exp = ((bits >> 52) & 0x7ff) as i32;
            let mant = bits & ((1 << 52) - 1);
            let text = if value == 0.0 {
                "0x0.0p0".to_string()
            } else if exp == 0 {
                format!("0x0.{}p-1022", trim_hex(mant))
            } else {
                format!("0x1.{}p{}", trim_hex(mant), exp - 1023)
            };
            if negative { format!("-{text}") } else { text }
        }
    })
}

fn trim_hex(mant: u64) -> String {
    let s = format!("{mant:013x}");
    let t = s.trim_end_matches('0');
    if t.is_empty() { "0".into() } else { t.into() }
}

/// The shortest decimal digits of a non-negative value and its decimal
/// exponent (value = 0.d1d2... * 10^(exp+1)), as `Double.toString` finds them.
fn shortest_digits(v: f64) -> (String, i32) {
    if v == 0.0 {
        return ("0".into(), 0);
    }
    let s = format!("{v:e}");
    split_sci(&s)
}

fn shortest_digits_f32(v: f32) -> (String, i32) {
    if v == 0.0 {
        return ("0".into(), 0);
    }
    let s = format!("{v:e}");
    split_sci(&s)
}

fn split_sci(s: &str) -> (String, i32) {
    let (m, e) = s.split_once('e').unwrap_or((s, "0"));
    let digits: String = m.chars().filter(char::is_ascii_digit).collect();
    (digits, e.parse().unwrap_or(0))
}

/// Digits rounded half-up to `n` significant digits, with the exponent the
/// rounding may raise.
fn round_digits(d: &(String, i32), n: usize) -> (String, i32) {
    let (digits, exp) = d;
    if digits.len() <= n {
        return (digits.clone(), *exp);
    }
    let mut kept: Vec<u8> = digits.as_bytes()[..n].to_vec();
    if digits.as_bytes()[n] >= b'5' {
        let mut i = n;
        loop {
            if i == 0 {
                kept.insert(0, b'1');
                kept.pop();
                return (String::from_utf8(kept).unwrap(), exp + 1);
            }
            i -= 1;
            if kept[i] == b'9' {
                kept[i] = b'0';
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    (String::from_utf8(kept).unwrap(), *exp)
}

/// `%.pf`: the digits rounded half-up at `p` places after the point.
/// `String.format("%.pf", v)` of a finite double.
pub fn fixed_point(v: f64, p: usize) -> String {
    let body = fixed(&shortest_digits(v.abs()), p);
    if v.is_sign_negative() { format!("-{body}") } else { body }
}

fn fixed(d: &(String, i32), p: usize) -> String {
    let (digits, exp) = d;
    // Position of the decimal point within the digits.
    let point = exp + 1;
    let keep = point + p as i32;
    let (digits, point) = if keep < 0 {
        ("0".to_string(), 1)
    } else if (keep as usize) < digits.len() {
        let (r, e) = round_digits(&(digits.clone(), *exp), keep.max(0) as usize);
        if keep == 0 {
            // Everything rounds away or up to one unit of the first place.
            let up = digits.as_bytes()[0] >= b'5';
            return if p == 0 {
                if up { format!("1{}", "0".repeat(point.max(0) as usize)) } else { "0".to_string() }
            } else {
                format!("0.{}{}", "0".repeat(p - 1), if up { "1" } else { "0" })
            };
        }
        (r, e + 1)
    } else {
        (digits.clone(), point)
    };
    let mut int_part = String::new();
    let mut frac = String::new();
    for (i, c) in digits.chars().enumerate() {
        if (i as i32) < point {
            int_part.push(c);
        } else {
            frac.push(c);
        }
    }
    if point > digits.len() as i32 {
        int_part.push_str(&"0".repeat(point as usize - digits.len()));
    }
    if point < 0 {
        frac = "0".repeat((-point) as usize) + &frac;
    }
    if int_part.is_empty() {
        int_part.push('0');
    }
    let int_part = int_part.trim_start_matches('0');
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    while frac.len() < p {
        frac.push('0');
    }
    frac.truncate(p);
    if p == 0 { int_part.to_string() } else { format!("{int_part}.{frac}") }
}

/// `%.pe`: one digit, `p` more after the point, `e+NN`.
fn scientific(d: &(String, i32), p: usize) -> String {
    if d.0 == "0" {
        return format!("{}e+00", if p == 0 { "0".to_string() } else { format!("0.{}", "0".repeat(p)) });
    }
    let (digits, exp) = round_digits(d, p + 1);
    let mut digits = digits;
    while digits.len() < p + 1 {
        digits.push('0');
    }
    let mantissa = if p == 0 { digits[..1].to_string() } else { format!("{}.{}", &digits[..1], &digits[1..p + 1]) };
    format!("{mantissa}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rounds_the_shortest_digits_half_up() {
        // Expected text is HotSpot 25's String.format.
        assert_eq!(fixed(&shortest_digits(2.675), 2), "2.68");
        assert_eq!(fixed(&shortest_digits(0.5), 0), "1");
        assert_eq!(fixed(&shortest_digits(1.5), 0), "2");
        assert_eq!(fixed(&shortest_digits(2.5), 0), "3");
        assert_eq!(fixed(&shortest_digits(0.25), 1), "0.3");
        assert_eq!(fixed(&shortest_digits(0.35), 1), "0.4");
        assert_eq!(fixed(&shortest_digits(std::f64::consts::PI), 6), "3.141593");
        assert_eq!(fixed(&shortest_digits(9876543.215), 2), "9876543.22");
        assert_eq!(fixed(&shortest_digits(0.05), 1), "0.1");
        assert_eq!(fixed(&shortest_digits(1.0005), 3), "1.001");
        assert_eq!(fixed(&shortest_digits(0.0), 2), "0.00");
        assert_eq!(fixed(&shortest_digits(1234.5), 2), "1234.50");
    }

    #[test]
    fn scientific_matches_java() {
        assert_eq!(scientific(&shortest_digits(12345.678), 6), "1.234568e+04");
        assert_eq!(scientific(&shortest_digits(0.000123), 3), "1.230e-04");
    }

    #[test]
    fn grouping_and_signs() {
        assert_eq!(signed(false, "1234567".into(), ",", None, true), "1,234,567");
        assert_eq!(signed(true, "3.14".into(), "(", None, true), "(3.14)");
        assert_eq!(signed(false, "42".into(), "0", Some(5), true), "00042");
        assert_eq!(signed(false, "42".into(), "+", None, true), "+42");
    }
}

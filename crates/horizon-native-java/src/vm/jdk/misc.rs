//! Smaller JDK classes mods and their libraries use: `Locale`, `Properties`,
//! checksums, `SplittableRandom`, `java.lang.IO`, `ScopedValue`, and the
//! annotation enums.
use super::{FINAL, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Data, Exc, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

// ------------------------------------------------------------------ Locale

/// A `Locale`'s language, country and variant strings.
fn locale_parts(vm: &Vm, r: ObjId) -> (String, String, String) {
    match state(vm, r) {
        Some(State::Generic(v)) if v.len() == 3 => (
            vm.string(v[0].r()).unwrap_or_default(),
            vm.string(v[1].r()).unwrap_or_default(),
            vm.string(v[2].r()).unwrap_or_default(),
        ),
        _ => Default::default(),
    }
}

pub fn new_locale(vm: &mut Vm, language: &str, country: &str, variant: &str) -> Res<ObjId> {
    let c = vm.load("java/util/Locale")?;
    let r = vm.new_instance(c)?;
    // Java keeps the old ISO codes for these three languages.
    let language = match language.to_ascii_lowercase().as_str() {
        "he" => "iw".to_string(),
        "yi" => "ji".to_string(),
        "id" => "in".to_string(),
        l => l.to_string(),
    };
    let (l, cc, v) = (vm.intern(&language)?, vm.intern(&country.to_ascii_uppercase())?, vm.intern(variant)?);
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(l), Value::Ref(cc), Value::Ref(v)])));
    }
    Ok(r)
}

const LOCALES: &[(&str, &str, &str)] = &[
    ("ENGLISH", "en", ""),
    ("FRENCH", "fr", ""),
    ("GERMAN", "de", ""),
    ("ITALIAN", "it", ""),
    ("JAPANESE", "ja", ""),
    ("KOREAN", "ko", ""),
    ("CHINESE", "zh", ""),
    ("SIMPLIFIED_CHINESE", "zh", "CN"),
    ("TRADITIONAL_CHINESE", "zh", "TW"),
    ("FRANCE", "fr", "FR"),
    ("GERMANY", "de", "DE"),
    ("ITALY", "it", "IT"),
    ("JAPAN", "ja", "JP"),
    ("KOREA", "ko", "KR"),
    ("UK", "en", "GB"),
    ("US", "en", "US"),
    ("CANADA", "en", "CA"),
    ("CANADA_FRENCH", "fr", "CA"),
    ("ROOT", "", ""),
    ("CHINA", "zh", "CN"),
    ("PRC", "zh", "CN"),
    ("TAIWAN", "zh", "TW"),
];

fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "es" => "Spanish",
        "pt" => "Portuguese",
        "ru" => "Russian",
        "nl" => "Dutch",
        "pl" => "Polish",
        "sv" => "Swedish",
        "tr" => "Turkish",
        "uk" => "Ukrainian",
        "cs" => "Czech",
        "fi" => "Finnish",
        "da" => "Danish",
        "no" | "nb" => "Norwegian Bokmål",
        "ar" => "Arabic",
        "iw" => "Hebrew",
        "in" => "Indonesian",
        "hi" => "Hindi",
        "el" => "Greek",
        "hu" => "Hungarian",
        other => other,
    }
}

fn country_name(code: &str) -> &str {
    match code {
        "US" => "United States",
        "GB" => "United Kingdom",
        "CA" => "Canada",
        "FR" => "France",
        "DE" => "Germany",
        "IT" => "Italy",
        "JP" => "Japan",
        "KR" => "South Korea",
        "CN" => "China",
        "TW" => "Taiwan",
        "ES" => "Spain",
        "BR" => "Brazil",
        "PT" => "Portugal",
        "RU" => "Russia",
        "NL" => "Netherlands",
        "AU" => "Australia",
        "IN" => "India",
        "MX" => "Mexico",
        "PL" => "Poland",
        "SE" => "Sweden",
        "CH" => "Switzerland",
        "AT" => "Austria",
        "BE" => "Belgium",
        "UA" => "Ukraine",
        other => other,
    }
}

/// The default locale: `en`, as HotSpot's is under the harness's C locale.
fn default_locale(vm: &mut Vm) -> Res<ObjId> {
    let c = vm.load("java/util/Locale")?;
    vm.ensure_init(c)?;
    let current = vm.class(c).statics.iter().zip(&vm.class(c).static_fields).find(|(_, f)| &*f.name == "$DEFAULT").map(|(v, _)| v.r()).unwrap_or(0);
    if current != 0 {
        return Ok(current);
    }
    let r = new_locale(vm, "en", "", "")?;
    vm.pinned.push(r);
    vm.set_static(c, "$DEFAULT", Value::Ref(r));
    Ok(r)
}

/// `Locale.hashCode` as HotSpot 25 computes it (fitted to its values: `en`
/// 3241, `en_US` 3118343, `_US` 3742, `fr_FR` 3151512, `en_US_x` 96668753).
fn locale_hash(language: &str, country: &str, variant: &str) -> i32 {
    let h = |s: &str| super::string::java_hash(&s.encode_utf16().collect::<Vec<_>>());
    let mut hc = h(language);
    if !country.is_empty() {
        hc = hc.wrapping_mul(961).wrapping_add(h(country)).wrapping_add(1024);
    }
    if !variant.is_empty() {
        hc = hc.wrapping_mul(31).wrapping_add(h(variant));
    }
    hc
}

fn locale_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let text = |vm: &mut Vm, v: Value| -> Res<String> {
        if v.r() == 0 {
            return Err(vm.npe(None));
        }
        Ok(vm.string(v.r()).unwrap_or_default())
    };
    match op {
        "init" => {
            let t = this(vm, a)?;
            let l = text(vm, arg(a, 1))?;
            let c = if a.len() > 2 { text(vm, arg(a, 2))? } else { String::new() };
            let v = if a.len() > 3 { text(vm, arg(a, 3))? } else { String::new() };
            let made = new_locale(vm, &l, &c, &v)?;
            let data = vm.obj(made).map(|o| o.data.clone()).unwrap_or_default();
            if let Some(o) = vm.obj_mut(t) {
                o.data = data;
            }
            void()
        }
        "of" => {
            let l = text(vm, arg(a, 0))?;
            let c = if a.len() > 1 { text(vm, arg(a, 1))? } else { String::new() };
            let v = if a.len() > 2 { text(vm, arg(a, 2))? } else { String::new() };
            ret_ref(new_locale(vm, &l, &c, &v)?)
        }
        "forLanguageTag" => {
            let tag = text(vm, arg(a, 0))?;
            if tag == "und" {
                return ret_ref(new_locale(vm, "", "", "")?);
            }
            let mut parts = tag.split(['-', '_']);
            let l = parts.next().unwrap_or("").to_string();
            let mut c = String::new();
            for p in parts {
                if p.len() == 2 && p.chars().all(|x| x.is_ascii_alphabetic()) {
                    c = p.to_string();
                    break;
                }
            }
            ret_ref(new_locale(vm, &l, &c, "")?)
        }
        "getDefault" => ret_ref(default_locale(vm)?),
        "setDefault" => {
            let c = vm.load("java/util/Locale")?;
            if arg(a, 0).r() == 0 {
                return Err(vm.npe(Some("Can't set default locale to NULL")));
            }
            vm.pinned.push(arg(a, 0).r());
            vm.set_static(c, "$DEFAULT", arg(a, 0));
            void()
        }
        "getAvailableLocales" => {
            let mut items = Vec::new();
            for (_, l, c) in LOCALES {
                items.push(new_locale(vm, l, c, "")?);
            }
            ret_ref(vm.new_ref_array("Ljava/util/Locale;", &items)?)
        }
        _ => {
            let t = this(vm, a)?;
            let (l, c, v) = locale_parts(vm, t);
            match op {
                "getLanguage" => ret_str(vm, &l),
                "getCountry" => ret_str(vm, &c),
                "getVariant" => ret_str(vm, &v),
                "getScript" => ret_str(vm, ""),
                "toString" => {
                    let mut s = l.clone();
                    if !c.is_empty() || !v.is_empty() {
                        s.push('_');
                        s.push_str(&c);
                    }
                    if !v.is_empty() {
                        s.push('_');
                        s.push_str(&v);
                    }
                    ret_str(vm, &s)
                }
                "toLanguageTag" => {
                    let lang = match l.as_str() {
                        "" => "und",
                        "iw" => "he",
                        "ji" => "yi",
                        "in" => "id",
                        x => x,
                    };
                    let mut s = lang.to_string();
                    if !c.is_empty() {
                        s.push('-');
                        s.push_str(&c);
                    }
                    ret_str(vm, &s)
                }
                "getDisplayLanguage" => {
                    let n = language_name(&l).to_string();
                    ret_str(vm, &n)
                }
                "getDisplayCountry" => {
                    let n = country_name(&c).to_string();
                    ret_str(vm, &n)
                }
                "getDisplayName" => {
                    let lang = language_name(&l).to_string();
                    let country = country_name(&c).to_string();
                    let s = match (lang.is_empty(), country.is_empty()) {
                        (false, false) => format!("{lang} ({country})"),
                        (false, true) => lang,
                        (true, false) => country,
                        _ => String::new(),
                    };
                    ret_str(vm, &s)
                }
                "equals" => {
                    let o = arg(a, 1).r();
                    let same = o != 0 && vm.class_of(o) == vm.class_of(t) && locale_parts(vm, o) == (l, c, v);
                    ret_bool(same)
                }
                "hashCode" => Ok(Value::Int(locale_hash(&l, &c, &v))),
                "stripExtensions" | "clone" => ret_ref(t),
                _ => Err(Exc::Unsupported(format!("Locale.{op}"))),
            }
        }
    }
}

fn locale_clinit(vm: &mut Vm, _: &[Value]) -> Res<Value> {
    let c = vm.load("java/util/Locale")?;
    for (name, l, cc) in LOCALES {
        let r = new_locale(vm, l, cc, "")?;
        vm.pinned.push(r);
        vm.set_static(c, name, Value::Ref(r));
    }
    void()
}

// ------------------------------------------------------------------ Properties

/// `Properties.load`: the format's escapes, separators and continuations.
fn parse_properties(text: &[u16]) -> Vec<(Vec<u16>, Vec<u16>)> {
    let text = String::from_utf16_lossy(text);
    let mut out = Vec::new();
    let mut logical = String::new();
    let mut lines = text.split(['\n']).map(|l| l.strip_suffix('\r').unwrap_or(l)).peekable();
    while let Some(raw) = lines.next() {
        let line = raw.trim_start_matches([' ', '\t', '\u{c}']);
        if logical.is_empty() && (line.is_empty() || line.starts_with('#') || line.starts_with('!')) {
            continue;
        }
        // An odd number of trailing backslashes continues the line.
        let trailing = line.chars().rev().take_while(|&c| c == '\\').count();
        if trailing % 2 == 1 {
            logical.push_str(&line[..line.len() - 1]);
            continue;
        }
        logical.push_str(line);
        let entry = std::mem::take(&mut logical);
        let mut key = String::new();
        let mut chars = entry.chars().peekable();
        let mut escaped = false;
        while let Some(&c) = chars.peek() {
            if escaped {
                key.push('\\');
                key.push(c);
                escaped = false;
                chars.next();
                continue;
            }
            if c == '\\' {
                escaped = true;
                chars.next();
                continue;
            }
            if matches!(c, '=' | ':' | ' ' | '\t' | '\u{c}') {
                break;
            }
            key.push(c);
            chars.next();
        }
        while chars.peek().is_some_and(|c| matches!(c, ' ' | '\t' | '\u{c}')) {
            chars.next();
        }
        if chars.peek().is_some_and(|c| matches!(c, '=' | ':')) {
            chars.next();
        }
        while chars.peek().is_some_and(|c| matches!(c, ' ' | '\t' | '\u{c}')) {
            chars.next();
        }
        let value: String = chars.collect();
        out.push((unescape(&key), unescape(&value)));
    }
    out
}

fn unescape(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buf));
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t' as u16),
            Some('n') => out.push('\n' as u16),
            Some('r') => out.push('\r' as u16),
            Some('f') => out.push(0x0C),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(u16::from_str_radix(&hex, 16).unwrap_or(0));
            }
            Some(x) => {
                let mut buf = [0u16; 2];
                out.extend_from_slice(x.encode_utf16(&mut buf));
            }
            None => {}
        }
    }
    out
}

/// `Properties.store`'s escaping of a key or value.
fn escape(units: &[u16], key: bool) -> String {
    let mut out = String::new();
    for (i, &u) in units.iter().enumerate() {
        match u {
            0x20 if key || i == 0 => out.push_str("\\ "),
            0x5C => out.push_str("\\\\"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x0C => out.push_str("\\f"),
            0x3D | 0x3A | 0x23 | 0x21 => {
                out.push('\\');
                out.push(u as u8 as char);
            }
            0x20..=0x7E => out.push(u as u8 as char),
            _ => out.push_str(&format!("\\u{u:04X}")),
        }
    }
    out
}

fn properties_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let get = |vm: &mut Vm, k: Value| -> Res<Value> {
        let mut map = t;
        loop {
            let v = super::util::map_get(vm, map, k)?;
            if let Some(v) = v
                && vm.string_units(v.r()).is_some()
            {
                return Ok(v);
            }
            let defaults = vm.jdk.property_defaults.get(&map).copied().unwrap_or(0);
            if defaults == 0 {
                return Ok(Value::NULL);
            }
            map = defaults;
        }
    };
    match op {
        "initDefaults" => {
            let d = arg(a, 1).r();
            if d != 0 {
                vm.jdk.property_defaults.insert(t, d);
            }
            void()
        }
        "getProperty" => get(vm, arg(a, 1)),
        "getPropertyDefault" => {
            let v = get(vm, arg(a, 1))?;
            Ok(if v.r() == 0 { arg(a, 2) } else { v })
        }
        "setProperty" => Ok(super::util::map_put(vm, t, arg(a, 1), arg(a, 2))?.unwrap_or(Value::NULL)),
        "load" => {
            let src = arg(a, 1).r();
            if src == 0 {
                return Err(vm.npe(None));
            }
            let reader = vm.load("java/io/Reader")?;
            let units = if vm.instance_of(src, reader) {
                read_reader(vm, src)?
            } else {
                let bytes = super::io::read_all_bytes(vm, src)?;
                // InputStreams are ISO 8859-1.
                bytes.iter().map(|&b| b as u16).collect()
            };
            for (k, v) in parse_properties(&units) {
                let (k, v) = (vm.new_string_units(k)?, vm.new_string_units(v)?);
                super::util::map_put(vm, t, Value::Ref(k), Value::Ref(v))?;
            }
            void()
        }
        "store" => {
            let target = arg(a, 1).r();
            let mut text = String::new();
            if arg(a, 2).r() != 0 {
                let c = vm.string(arg(a, 2).r()).unwrap_or_default();
                text.push_str(&format!("#{c}\n"));
            }
            text.push_str("#Thu Jan 01 00:00:00 UTC 2026\n");
            for (k, v) in super::util::map_pairs(vm, t)? {
                let (ku, vu) = (vm.string_units(k.r()).unwrap_or(&[]).to_vec(), vm.string_units(v.r()).unwrap_or(&[]).to_vec());
                text.push_str(&format!("{}={}\n", escape(&ku, true), escape(&vu, false)));
            }
            let units: Vec<u16> = text.encode_utf16().collect();
            let writer = vm.load("java/io/Writer")?;
            if vm.instance_of(target, writer) {
                super::io::write_chars(vm, target, &units)?;
            } else {
                super::io::write_bytes(vm, target, &super::io::encode(&units, "ISO-8859-1"))?;
            }
            void()
        }
        "stringPropertyNames" | "propertyNames" => {
            let mut names = Vec::new();
            let mut map = t;
            loop {
                for (k, v) in super::util::map_pairs(vm, map)? {
                    if vm.string_units(k.r()).is_some() && vm.string_units(v.r()).is_some() {
                        let mut dup = false;
                        for n in &names {
                            if super::util::equals(vm, *n, k)? {
                                dup = true;
                            }
                        }
                        if !dup {
                            names.push(k);
                        }
                    }
                }
                map = vm.jdk.property_defaults.get(&map).copied().unwrap_or(0);
                if map == 0 {
                    break;
                }
            }
            let set = super::util::new_map(vm, "java/util/HashSet")?;
            if let Some(State::Map(m)) = super::state_mut(vm, set) {
                m.set = true;
            }
            for n in names {
                super::util::map_put(vm, set, n, Value::Int(1))?;
            }
            if op == "propertyNames" {
                let items = super::util::iterate(vm, set)?;
                let l = super::util::new_list(vm, items)?;
                return vm.invoke_static("java/util/Collections", "enumeration", "(Ljava/util/Collection;)Ljava/util/Enumeration;", &[Value::Ref(l)]);
            }
            ret_ref(set)
        }
        "list" => {
            let target = arg(a, 1).r();
            let mut text = String::from("-- listing properties --\n");
            for (k, v) in super::util::map_pairs(vm, t)? {
                let k = String::from_utf16_lossy(&vm.stringify(k, "Ljava/lang/Object;")?);
                let mut v = String::from_utf16_lossy(&vm.stringify(v, "Ljava/lang/Object;")?);
                if v.chars().count() > 40 {
                    v = format!("{}...", v.chars().take(37).collect::<String>());
                }
                text.push_str(&format!("{k}={v}\n"));
            }
            let units: Vec<u16> = text.encode_utf16().collect();
            super::io::write_chars(vm, target, &units)?;
            void()
        }
        _ => Err(Exc::Unsupported(format!("Properties.{op}"))),
    }
}

fn read_reader(vm: &mut Vm, r: ObjId) -> Res<Vec<u16>> {
    super::io::reader_rest(vm, r)
}

// ------------------------------------------------------------------ checksums

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// `CRC32` (`adler` false) or `Adler32` over bytes, continuing from `value`.
fn checksum(value: u32, bytes: &[u8], adler: bool) -> u32 {
    if adler {
        let (mut a, mut b) = (value & 0xFFFF, value >> 16);
        for &x in bytes {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    } else {
        let t = crc32_table();
        let mut c = !value;
        for &x in bytes {
            c = t[((c ^ x as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        !c
    }
}

fn checksum_op(vm: &mut Vm, a: &[Value], op: &str, adler: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let current = match state(vm, t) {
        Some(State::Atomic(v)) => v.i() as u32,
        _ => u32::from(adler),
    };
    let set = |vm: &mut Vm, v: u32| {
        if let Some(o) = vm.obj_mut(t) {
            o.data = Data::Jdk(Box::new(State::Atomic(Value::Int(v as i32))));
        }
    };
    match op {
        "init" | "reset" => {
            set(vm, u32::from(adler));
            void()
        }
        "updateByte" => {
            set(vm, checksum(current, &[arg(a, 1).i() as u8], adler));
            void()
        }
        "updateBytes" => {
            let b = super::string::bytes_of(vm, arg(a, 1).r())?;
            let (s, e) = if a.len() > 2 {
                let (off, n) = (arg(a, 2).i(), arg(a, 3).i());
                if off < 0 || n < 0 || off as usize + n as usize > b.len() {
                    return Err(vm.throw_new("java/lang/ArrayIndexOutOfBoundsException", None));
                }
                (off as usize, off as usize + n as usize)
            } else {
                (0, b.len())
            };
            set(vm, checksum(current, &b[s..e], adler));
            void()
        }
        "getValue" => Ok(Value::Long(current as i64)),
        _ => Err(Exc::Unsupported(format!("checksum {op}"))),
    }
}

// ------------------------------------------------------------------ SplittableRandom

const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

fn mix64(z: u64) -> u64 {
    let z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn mix32(z: u64) -> i32 {
    let z = (z ^ (z >> 33)).wrapping_mul(0x62a9_d9ed_7997_05f5);
    ((z ^ (z >> 28)).wrapping_mul(0xcb24_d0a5_c88c_35b3) >> 32) as i32
}

fn mix_gamma(z: u64) -> u64 {
    let z = (z ^ (z >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
    let z = (z ^ (z >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    let z = (z ^ (z >> 33)) | 1;
    if (z ^ (z >> 1)).count_ones() < 24 { z ^ 0xaaaa_aaaa_aaaa_aaaa } else { z }
}

fn split_state(vm: &Vm, r: ObjId) -> (u64, u64) {
    match state(vm, r) {
        Some(State::Generic(v)) if v.len() == 2 => (v[0].j() as u64, v[1].j() as u64),
        _ => (0, GOLDEN_GAMMA),
    }
}

fn set_split(vm: &mut Vm, r: ObjId, seed: u64, gamma: u64) {
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Long(seed as i64), Value::Long(gamma as i64)])));
    }
}

fn next_seed(vm: &mut Vm, r: ObjId) -> u64 {
    let (seed, gamma) = split_state(vm, r);
    let s = seed.wrapping_add(gamma);
    set_split(vm, r, s, gamma);
    s
}

fn split_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let next_int = |vm: &mut Vm| mix32(next_seed(vm, t));
    let next_long = |vm: &mut Vm| mix64(next_seed(vm, t)) as i64;
    match op {
        "init" => {
            let seed = match a.get(1) {
                Some(Value::Long(s)) => *s as u64,
                // The default generator's seed: fixed, so runs repeat.
                _ => mix64(vm.instructions ^ 0x5DEE_CE66D),
            };
            set_split(vm, t, seed, GOLDEN_GAMMA);
            void()
        }
        "nextInt" => Ok(Value::Int(next_int(vm))),
        "nextLong" => Ok(Value::Long(next_long(vm))),
        "nextIntBound" => {
            let bound = arg(a, 1).i();
            if bound <= 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("bound must be positive")));
            }
            let mut r = next_int(vm);
            let m = bound - 1;
            if bound & m == 0 {
                r &= m;
            } else {
                let mut u = ((r as u32) >> 1) as i32;
                loop {
                    r = u % bound;
                    if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                        break;
                    }
                    u = ((next_int(vm) as u32) >> 1) as i32;
                }
            }
            Ok(Value::Int(r))
        }
        "nextIntRange" => {
            let (origin, bound) = (arg(a, 1).i(), arg(a, 2).i());
            if origin >= bound {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("bound must be greater than origin")));
            }
            let mut r = next_int(vm);
            let n = bound.wrapping_sub(origin);
            let m = n.wrapping_sub(1);
            if n & m == 0 {
                r = (r & m).wrapping_add(origin);
            } else if n > 0 {
                let mut u = ((r as u32) >> 1) as i32;
                loop {
                    r = u % n;
                    if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                        break;
                    }
                    u = ((next_int(vm) as u32) >> 1) as i32;
                }
                r = r.wrapping_add(origin);
            } else {
                while r < origin || r >= bound {
                    r = next_int(vm);
                }
            }
            Ok(Value::Int(r))
        }
        "nextDouble" => Ok(Value::Double(((next_long(vm) as u64) >> 11) as f64 * (1.0 / (1u64 << 53) as f64))),
        "nextFloat" => Ok(Value::Float(((next_int(vm) as u32) >> 8) as f32 * (1.0 / (1u32 << 24) as f32))),
        "nextBoolean" => ret_bool(next_int(vm) < 0),
        "split" => {
            let seed = next_long(vm) as u64;
            let gamma = mix_gamma(next_seed(vm, t));
            let c = vm.load("java/util/SplittableRandom")?;
            let r = vm.new_instance(c)?;
            set_split(vm, r, seed, gamma);
            ret_ref(r)
        }
        _ => Err(Exc::Unsupported(format!("SplittableRandom.{op}"))),
    }
}

// ------------------------------------------------------------------ java.lang.IO

fn io_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let out = vm.get_static("java/lang/System", "out", "Ljava/io/PrintStream;")?.r();
    match op {
        "println" => vm.invoke_virtual(out, "println", "(Ljava/lang/Object;)V", &[arg(a, 0)]).map(|_| Value::Top),
        "println0" => vm.invoke_virtual(out, "println", "()V", &[]).map(|_| Value::Top),
        "print" => vm.invoke_virtual(out, "print", "(Ljava/lang/Object;)V", &[arg(a, 0)]).map(|_| Value::Top),
        "readln" => {
            if !a.is_empty() {
                vm.invoke_virtual(out, "print", "(Ljava/lang/Object;)V", &[arg(a, 0)])?;
            }
            ret_ref(0)
        }
        _ => Err(Exc::Unsupported(format!("IO.{op}"))),
    }
}

// ------------------------------------------------------------------ ScopedValue

fn scoped_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "newInstance" => {
            let c = vm.load("java/lang/ScopedValue")?;
            ret_ref(vm.new_instance(c)?)
        }
        "where" | "carrierWhere" => {
            // A carrier: its bindings (pairs of value object and value).
            let (mut bindings, sv, v) = if op == "where" {
                (Vec::new(), arg(a, 0), arg(a, 1))
            } else {
                let t = this(vm, a)?;
                let existing = match state(vm, t) {
                    Some(State::Generic(b)) => b.clone(),
                    _ => Vec::new(),
                };
                (existing, arg(a, 1), arg(a, 2))
            };
            if sv.r() == 0 {
                return Err(vm.npe(None));
            }
            bindings.push(sv);
            bindings.push(v);
            let c = vm.load("java/lang/ScopedValue$Carrier")?;
            let r = vm.new_instance(c)?;
            if let Some(o) = vm.obj_mut(r) {
                o.data = Data::Jdk(Box::new(State::Generic(bindings)));
            }
            ret_ref(r)
        }
        "run" | "call" => {
            let t = this(vm, a)?;
            let bindings = match state(vm, t) {
                Some(State::Generic(b)) => b.clone(),
                _ => Vec::new(),
            };
            let depth = vm.jdk.scoped.len();
            let thread = vm.current_thread;
            for pair in bindings.chunks(2) {
                vm.jdk.scoped.push((thread, pair[0].r(), pair[1]));
            }
            let result = if op == "run" {
                vm.invoke_virtual(arg(a, 1).r(), "run", "()V", &[]).map(|_| Value::Top)
            } else {
                vm.invoke_virtual(arg(a, 1).r(), "call", "()Ljava/lang/Object;", &[])
            };
            vm.jdk.scoped.truncate(depth);
            result
        }
        "get" | "isBound" | "orElse" | "orElseThrow" => {
            let t = this(vm, a)?;
            let thread = vm.current_thread;
            let found = vm.jdk.scoped.iter().rev().find(|(th, sv, _)| *sv == t && *th == thread).map(|(_, _, v)| *v);
            match (op, found) {
                ("isBound", f) => ret_bool(f.is_some()),
                (_, Some(v)) => Ok(v),
                ("orElse", None) => Ok(arg(a, 1)),
                ("orElseThrow", None) => {
                    let ex = vm.invoke_virtual(arg(a, 1).r(), "get", "()Ljava/lang/Object;", &[])?.r();
                    Err(Exc::Throw(ex))
                }
                _ => Err(vm.throw_new("java/util/NoSuchElementException", Some("ScopedValue not bound"))),
            }
        }
        _ => Err(Exc::Unsupported(format!("ScopedValue.{op}"))),
    }
}

// ------------------------------------------------------------------ UUID

/// MD5 (RFC 1321), for `UUID.nameUUIDFromBytes`.
pub fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16,
        23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    let mut msg = input.to_vec();
    let bits = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, w) in [a0, b0, c0, d0].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn uuid_bits(vm: &Vm, r: ObjId) -> (i64, i64) {
    match state(vm, r) {
        Some(State::Generic(v)) if v.len() == 2 => (v[0].j(), v[1].j()),
        _ => (0, 0),
    }
}

fn new_uuid(vm: &mut Vm, msb: i64, lsb: i64) -> Res<ObjId> {
    let c = vm.load("java/util/UUID")?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Long(msb), Value::Long(lsb)])));
    }
    Ok(r)
}

fn uuid_from_bytes(b: [u8; 16]) -> (i64, i64) {
    let msb = b[..8].iter().fold(0u64, |acc, &x| (acc << 8) | x as u64);
    let lsb = b[8..].iter().fold(0u64, |acc, &x| (acc << 8) | x as u64);
    (msb as i64, lsb as i64)
}

fn uuid_text(msb: i64, lsb: i64) -> String {
    let (m, l) = (msb as u64, lsb as u64);
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", m >> 32, (m >> 16) & 0xFFFF, m & 0xFFFF, l >> 48, l & 0xFFFF_FFFF_FFFF)
}

fn uuid_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "init" => {
            let t = this(vm, a)?;
            if let Some(o) = vm.obj_mut(t) {
                o.data = Data::Jdk(Box::new(State::Generic(vec![arg(a, 1), arg(a, 2)])));
            }
            void()
        }
        "randomUUID" => {
            // Version 4 from the VM's own generator: random, and the same every run.
            let r = super::util::shared_random(vm)?;
            let mut b = [0u8; 16];
            for chunk in b.chunks_mut(4) {
                let v = vm.invoke_virtual(r, "nextInt", "()I", &[])?.i();
                chunk.copy_from_slice(&v.to_be_bytes());
            }
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            let (m, l) = uuid_from_bytes(b);
            ret_ref(new_uuid(vm, m, l)?)
        }
        "nameUUIDFromBytes" => {
            let bytes = super::string::bytes_of(vm, arg(a, 0).r())?;
            let mut b = md5(&bytes);
            b[6] = (b[6] & 0x0f) | 0x30;
            b[8] = (b[8] & 0x3f) | 0x80;
            let (m, l) = uuid_from_bytes(b);
            ret_ref(new_uuid(vm, m, l)?)
        }
        "fromString" => {
            let s = vm.string_nn(arg(a, 0).r())?;
            let parts: Vec<&str> = s.split('-').collect();
            let bad = |vm: &mut Vm| vm.throw_new("java/lang/IllegalArgumentException", Some(&format!("Invalid UUID string: {s}")));
            if parts.len() != 5 || s.len() > 36 {
                return Err(bad(vm));
            }
            let mut n = [0u64; 5];
            for (i, p) in parts.iter().enumerate() {
                match u64::from_str_radix(p, 16) {
                    Ok(v) if !p.is_empty() => n[i] = v,
                    _ => return Err(bad(vm)),
                }
            }
            let msb = ((n[0] & 0xFFFF_FFFF) << 32) | ((n[1] & 0xFFFF) << 16) | (n[2] & 0xFFFF);
            let lsb = ((n[3] & 0xFFFF) << 48) | (n[4] & 0xFFFF_FFFF_FFFF);
            ret_ref(new_uuid(vm, msb as i64, lsb as i64)?)
        }
        _ => {
            let t = this(vm, a)?;
            let (m, l) = uuid_bits(vm, t);
            match op {
                "getMostSignificantBits" => Ok(Value::Long(m)),
                "getLeastSignificantBits" => Ok(Value::Long(l)),
                "version" => Ok(Value::Int(((m >> 12) & 0x0f) as i32)),
                "variant" => {
                    // (lsb >>> (64 - (lsb >>> 62))) & (lsb >> 63), with Java's shift masking.
                    let shift = (64 - ((l as u64) >> 62)) & 63;
                    Ok(Value::Int((((l as u64) >> shift) as i64 & (l >> 63)) as i32))
                }
                "toString" => ret_str(vm, &uuid_text(m, l)),
                "hashCode" => {
                    let hilo = m ^ l;
                    Ok(Value::Int(((hilo >> 32) as i32) ^ (hilo as i32)))
                }
                "equals" => {
                    let o = arg(a, 1).r();
                    ret_bool(o != 0 && vm.class_of(o) == vm.class_of(t) && uuid_bits(vm, o) == (m, l))
                }
                "compareTo" => {
                    let (om, ol) = uuid_bits(vm, arg(a, 1).r());
                    Ok(Value::Int(m.cmp(&om).then(l.cmp(&ol)) as i32))
                }
                _ => Err(Exc::Unsupported(format!("UUID.{op}"))),
            }
        }
    }
}

// ------------------------------------------------------------------ HexFormat

/// A `HexFormat`: delimiter, prefix, suffix, upper case.
fn hex_parts(vm: &Vm, r: ObjId) -> (String, String, String, bool) {
    match state(vm, r) {
        Some(State::Generic(v)) if v.len() == 4 => (vm.string(v[0].r()).unwrap_or_default(), vm.string(v[1].r()).unwrap_or_default(), vm.string(v[2].r()).unwrap_or_default(), v[3].z()),
        _ => (String::new(), String::new(), String::new(), false),
    }
}

fn new_hex(vm: &mut Vm, d: &str, p: &str, s: &str, upper: bool) -> Res<Value> {
    let c = vm.load("java/util/HexFormat")?;
    let r = vm.new_instance(c)?;
    let (d, p, s) = (vm.intern(d)?, vm.intern(p)?, vm.intern(s)?);
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(d), Value::Ref(p), Value::Ref(s), Value::Int(upper as i32)])));
    }
    ret_ref(r)
}

fn hex_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "of" => return new_hex(vm, "", "", "", false),
        "ofDelimiter" => {
            let d = vm.string_nn(arg(a, 0).r())?;
            return new_hex(vm, &d, "", "", false);
        }
        "isHexDigit" => return ret_bool((arg(a, 0).i() as u8 as char).is_ascii_hexdigit() && arg(a, 0).i() < 128),
        "fromHexDigits" | "fromHexDigitsToLong" | "fromHexDigit" => {
            let text = if op == "fromHexDigit" { ((arg(a, 0).i() as u8) as char).to_string() } else { vm.string_nn(arg(a, 0).r())? };
            let max = if op == "fromHexDigitsToLong" { 16 } else { 8 };
            if text.len() > max || text.is_empty() {
                let msg = format!("string length greater than {max}: {}", text.len());
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
            }
            return match u64::from_str_radix(&text, 16) {
                Ok(v) if op == "fromHexDigitsToLong" => Ok(Value::Long(v as i64)),
                Ok(v) => Ok(Value::Int(v as u32 as i32)),
                Err(_) => Err(vm.throw_new("java/lang/NumberFormatException", Some(&format!("not a hexadecimal digit: \"{text}\"")))),
            };
        }
        _ => {}
    }
    let t = this(vm, a)?;
    let (d, p, s, upper) = hex_parts(vm, t);
    let case = |x: String| if upper { x.to_uppercase() } else { x };
    match op {
        "withUpperCase" => new_hex(vm, &d, &p, &s, true),
        "withLowerCase" => new_hex(vm, &d, &p, &s, false),
        "withDelimiter" => {
            let n = vm.string_nn(arg(a, 1).r())?;
            new_hex(vm, &n, &p, &s, upper)
        }
        "withPrefix" => {
            let n = vm.string_nn(arg(a, 1).r())?;
            new_hex(vm, &d, &n, &s, upper)
        }
        "withSuffix" => {
            let n = vm.string_nn(arg(a, 1).r())?;
            new_hex(vm, &d, &p, &n, upper)
        }
        "delimiter" => ret_str(vm, &d),
        "prefix" => ret_str(vm, &p),
        "suffix" => ret_str(vm, &s),
        "isUpperCase" => ret_bool(upper),
        "formatHex" => {
            let b = super::string::bytes_of(vm, arg(a, 1).r())?;
            let (st, en) = if a.len() > 2 { (arg(a, 2).i().max(0) as usize, arg(a, 3).i().max(0) as usize) } else { (0, b.len()) };
            let parts: Vec<String> = b[st.min(b.len())..en.min(b.len())].iter().map(|x| format!("{p}{}{s}", case(format!("{x:02x}")))).collect();
            ret_str(vm, &parts.join(&d))
        }
        "parseHex" => {
            let text = String::from_utf16_lossy(&super::string::units(vm, arg(a, 1).r())?);
            let mut out = Vec::new();
            let mut rest = text.as_str();
            let mut first = true;
            while !rest.is_empty() {
                if !first {
                    rest = match rest.strip_prefix(d.as_str()) {
                        Some(r) => r,
                        None => return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("found: missing delimiter"))),
                    };
                }
                first = false;
                rest = rest.strip_prefix(p.as_str()).unwrap_or(rest);
                let (hex, after) = rest.split_at(2.min(rest.len()));
                match u8::from_str_radix(hex, 16) {
                    Ok(v) if hex.len() == 2 => out.push(v),
                    _ => return Err(vm.throw_new("java/lang/NumberFormatException", Some(&format!("not a hexadecimal digit: \"{hex}\"")))),
                }
                rest = after.strip_prefix(s.as_str()).unwrap_or(after);
            }
            super::string::bytes_value(vm, out)
        }
        "toHexDigits" => {
            let v = arg(a, 1);
            let text = match (v, a.len()) {
                (Value::Long(x), _) => format!("{x:016x}"),
                (Value::Int(x), 3) => {
                    let digits = arg(a, 2).i().clamp(0, 16) as usize;
                    let full = format!("{:016x}", x as i64);
                    full[16 - digits..].to_string()
                }
                (Value::Int(x), _) => format!("{x:08x}"),
                _ => String::new(),
            };
            ret_str(vm, &case(text))
        }
        "toHexDigitsByte" => ret_str(vm, &case(format!("{:02x}", arg(a, 1).i() as u8))),
        "toHexDigitsShort" | "toHexDigitsChar" => ret_str(vm, &case(format!("{:04x}", arg(a, 1).i() as u16))),
        "toString" => {
            let text = format!("uppercase: {upper}, delimiter: \"{d}\", prefix: \"{p}\", suffix: \"{s}\"");
            ret_str(vm, &text)
        }
        _ => Err(Exc::Unsupported(format!("HexFormat.{op}"))),
    }
}

// ------------------------------------------------------------------ classes

macro_rules! jdk_enum {
    ($name:literal, [$($c:literal),* $(,)?]) => {
        NativeClass {
            name: $name,
            super_name: Some("java/lang/Enum"),
            interfaces: &[],
            access: PUBLIC | FINAL | 0x4000,
            methods: natives![
                "values" (concat!("()[L", $name, ";")) S => Some(|vm, _| super::math::enum_values(vm, $name)),
                "valueOf" (concat!("(Ljava/lang/String;)L", $name, ";")) S => Some(|vm, a| { let c = vm.load($name)?; let m = vm.mirror(c)?; vm.invoke_static("java/lang/Enum", "valueOf", "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;", &[Value::Ref(m), arg(a, 0)]) }),
            ],
            statics: &[$(($c, concat!("L", $name, ";")),)* ("$VALUES", concat!("[L", $name, ";"))],
            clinit: Some(|vm, _| super::math::make_enum_constants(vm, $name, &[$($c),*])),
            data: None,
        }
    };
}

static LOCALE_METHODS: &[NativeMethod] = natives![
    "<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| locale_op(vm, a, "init")),
    "<init>" "(Ljava/lang/String;Ljava/lang/String;)V" M => Some(|vm, a| locale_op(vm, a, "init")),
    "<init>" "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V" M => Some(|vm, a| locale_op(vm, a, "init")),
    "of" "(Ljava/lang/String;)Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "of")),
    "of" "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "of")),
    "of" "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "of")),
    "forLanguageTag" "(Ljava/lang/String;)Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "forLanguageTag")),
    "getDefault" "()Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "getDefault")),
    "getDefault" "(Ljava/util/Locale$Category;)Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "getDefault")),
    "setDefault" "(Ljava/util/Locale;)V" S => Some(|vm, a| locale_op(vm, a, "setDefault")),
    "getAvailableLocales" "()[Ljava/util/Locale;" S => Some(|vm, a| locale_op(vm, a, "getAvailableLocales")),
    "getLanguage" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getLanguage")),
    "getCountry" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getCountry")),
    "getVariant" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getVariant")),
    "getScript" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getScript")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "toString")),
    "toLanguageTag" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "toLanguageTag")),
    "getDisplayLanguage" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getDisplayLanguage")),
    "getDisplayLanguage" "(Ljava/util/Locale;)Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getDisplayLanguage")),
    "getDisplayCountry" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getDisplayCountry")),
    "getDisplayCountry" "(Ljava/util/Locale;)Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getDisplayCountry")),
    "getDisplayName" "()Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getDisplayName")),
    "getDisplayName" "(Ljava/util/Locale;)Ljava/lang/String;" M => Some(|vm, a| locale_op(vm, a, "getDisplayName")),
    "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| locale_op(vm, a, "equals")),
    "hashCode" "()I" M => Some(|vm, a| locale_op(vm, a, "hashCode")),
    "stripExtensions" "()Ljava/util/Locale;" M => Some(|vm, a| locale_op(vm, a, "stripExtensions")),
    "clone" "()Ljava/lang/Object;" M => Some(|vm, a| locale_op(vm, a, "clone")),
];

static CLASSES: &[NativeClass] = &[
    NativeClass {
        name: "java/util/Locale",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/lang/Cloneable", "java/io/Serializable"],
        access: PUBLIC | FINAL,
        methods: LOCALE_METHODS,
        statics: &[
            ("ENGLISH", "Ljava/util/Locale;"),
            ("FRENCH", "Ljava/util/Locale;"),
            ("GERMAN", "Ljava/util/Locale;"),
            ("ITALIAN", "Ljava/util/Locale;"),
            ("JAPANESE", "Ljava/util/Locale;"),
            ("KOREAN", "Ljava/util/Locale;"),
            ("CHINESE", "Ljava/util/Locale;"),
            ("SIMPLIFIED_CHINESE", "Ljava/util/Locale;"),
            ("TRADITIONAL_CHINESE", "Ljava/util/Locale;"),
            ("FRANCE", "Ljava/util/Locale;"),
            ("GERMANY", "Ljava/util/Locale;"),
            ("ITALY", "Ljava/util/Locale;"),
            ("JAPAN", "Ljava/util/Locale;"),
            ("KOREA", "Ljava/util/Locale;"),
            ("UK", "Ljava/util/Locale;"),
            ("US", "Ljava/util/Locale;"),
            ("CANADA", "Ljava/util/Locale;"),
            ("CANADA_FRENCH", "Ljava/util/Locale;"),
            ("ROOT", "Ljava/util/Locale;"),
            ("CHINA", "Ljava/util/Locale;"),
            ("PRC", "Ljava/util/Locale;"),
            ("TAIWAN", "Ljava/util/Locale;"),
            ("$DEFAULT", "Ljava/util/Locale;"),
        ],
        clinit: Some(locale_clinit),
        data: None,
    },
    jdk_enum!("java/util/Locale$Category", ["DISPLAY", "FORMAT"]),
    NativeClass {
        name: "java/util/Properties",
        super_name: Some("java/util/Hashtable"),
        interfaces: &[],
        access: PUBLIC,
        methods: natives![
            "<init>" "()V" M => Some(|vm, a| properties_op(vm, &a[..1], "initDefaults")),
            "<init>" "(I)V" M => Some(|vm, a| properties_op(vm, &a[..1], "initDefaults")),
            "<init>" "(Ljava/util/Properties;)V" M => Some(|vm, a| properties_op(vm, a, "initDefaults")),
            "getProperty" "(Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| properties_op(vm, a, "getProperty")),
            "getProperty" "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| properties_op(vm, a, "getPropertyDefault")),
            "setProperty" "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;" M => Some(|vm, a| properties_op(vm, a, "setProperty")),
            "load" "(Ljava/io/Reader;)V" M => Some(|vm, a| properties_op(vm, a, "load")),
            "load" "(Ljava/io/InputStream;)V" M => Some(|vm, a| properties_op(vm, a, "load")),
            "store" "(Ljava/io/Writer;Ljava/lang/String;)V" M => Some(|vm, a| properties_op(vm, a, "store")),
            "store" "(Ljava/io/OutputStream;Ljava/lang/String;)V" M => Some(|vm, a| properties_op(vm, a, "store")),
            "stringPropertyNames" "()Ljava/util/Set;" M => Some(|vm, a| properties_op(vm, a, "stringPropertyNames")),
            "propertyNames" "()Ljava/util/Enumeration;" M => Some(|vm, a| properties_op(vm, a, "propertyNames")),
            "list" "(Ljava/io/PrintStream;)V" M => Some(|vm, a| properties_op(vm, a, "list")),
            "list" "(Ljava/io/PrintWriter;)V" M => Some(|vm, a| properties_op(vm, a, "list")),
        ],
        statics: &[],
        clinit: None,
        data: None,
    },
    class("java/util/zip/Checksum", None, &[], super::INTERFACE, natives!["getValue" "()J" super::A => None]),
    NativeClass { name: "java/util/zip/CRC32", super_name: Some("java/lang/Object"), interfaces: &["java/util/zip/Checksum"], access: PUBLIC, methods: natives![
        "<init>" "()V" M => Some(|vm, a| checksum_op(vm, a, "init", false)),
        "update" "(I)V" M => Some(|vm, a| checksum_op(vm, a, "updateByte", false)),
        "update" "([B)V" M => Some(|vm, a| checksum_op(vm, a, "updateBytes", false)),
        "update" "([BII)V" M => Some(|vm, a| checksum_op(vm, a, "updateBytes", false)),
        "getValue" "()J" M => Some(|vm, a| checksum_op(vm, a, "getValue", false)),
        "reset" "()V" M => Some(|vm, a| checksum_op(vm, a, "reset", false)),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/zip/Adler32", super_name: Some("java/lang/Object"), interfaces: &["java/util/zip/Checksum"], access: PUBLIC, methods: natives![
        "<init>" "()V" M => Some(|vm, a| checksum_op(vm, a, "init", true)),
        "update" "(I)V" M => Some(|vm, a| checksum_op(vm, a, "updateByte", true)),
        "update" "([B)V" M => Some(|vm, a| checksum_op(vm, a, "updateBytes", true)),
        "update" "([BII)V" M => Some(|vm, a| checksum_op(vm, a, "updateBytes", true)),
        "getValue" "()J" M => Some(|vm, a| checksum_op(vm, a, "getValue", true)),
        "reset" "()V" M => Some(|vm, a| checksum_op(vm, a, "reset", true)),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/SplittableRandom", super_name: Some("java/lang/Object"), interfaces: &["java/util/random/RandomGenerator"], access: PUBLIC | FINAL, methods: natives![
        "<init>" "()V" M => Some(|vm, a| split_op(vm, a, "init")),
        "<init>" "(J)V" M => Some(|vm, a| split_op(vm, a, "init")),
        "nextInt" "()I" M => Some(|vm, a| split_op(vm, a, "nextInt")),
        "nextInt" "(I)I" M => Some(|vm, a| split_op(vm, a, "nextIntBound")),
        "nextInt" "(II)I" M => Some(|vm, a| split_op(vm, a, "nextIntRange")),
        "nextLong" "()J" M => Some(|vm, a| split_op(vm, a, "nextLong")),
        "nextDouble" "()D" M => Some(|vm, a| split_op(vm, a, "nextDouble")),
        "nextFloat" "()F" M => Some(|vm, a| split_op(vm, a, "nextFloat")),
        "nextBoolean" "()Z" M => Some(|vm, a| split_op(vm, a, "nextBoolean")),
        "split" "()Ljava/util/SplittableRandom;" M => Some(|vm, a| split_op(vm, a, "split")),
    ], statics: &[], clinit: None, data: None },
    class("java/lang/IO", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![
        "println" "(Ljava/lang/Object;)V" S => Some(|vm, a| io_op(vm, a, "println")),
        "println" "()V" S => Some(|vm, a| io_op(vm, a, "println0")),
        "print" "(Ljava/lang/Object;)V" S => Some(|vm, a| io_op(vm, a, "print")),
        "readln" "()Ljava/lang/String;" S => Some(|vm, a| io_op(vm, &a[..0], "readln")),
        "readln" "(Ljava/lang/String;)Ljava/lang/String;" S => Some(|vm, a| io_op(vm, a, "readln")),
    ]),
    class("java/lang/ScopedValue", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![
        "newInstance" "()Ljava/lang/ScopedValue;" S => Some(|vm, a| scoped_op(vm, a, "newInstance")),
        "where" "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;" S => Some(|vm, a| scoped_op(vm, a, "where")),
        "get" "()Ljava/lang/Object;" M => Some(|vm, a| scoped_op(vm, a, "get")),
        "isBound" "()Z" M => Some(|vm, a| scoped_op(vm, a, "isBound")),
        "orElse" "(Ljava/lang/Object;)Ljava/lang/Object;" M => Some(|vm, a| scoped_op(vm, a, "orElse")),
        "orElseThrow" "(Ljava/util/function/Supplier;)Ljava/lang/Object;" M => Some(|vm, a| scoped_op(vm, a, "orElseThrow")),
    ]),
    class("java/lang/ScopedValue$Carrier", Some("java/lang/Object"), &[], PUBLIC | FINAL, natives![
        "where" "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;" M => Some(|vm, a| scoped_op(vm, a, "carrierWhere")),
        "run" "(Ljava/lang/Runnable;)V" M => Some(|vm, a| scoped_op(vm, a, "run")),
        "call" "(Ljava/lang/ScopedValue$CallableOp;)Ljava/lang/Object;" M => Some(|vm, a| scoped_op(vm, a, "call")),
    ]),
    class("java/lang/ScopedValue$CallableOp", None, &[], super::INTERFACE, natives!["call" "()Ljava/lang/Object;" super::A => None]),
    NativeClass { name: "java/util/UUID", super_name: Some("java/lang/Object"), interfaces: &["java/io/Serializable", "java/lang/Comparable"], access: PUBLIC | FINAL, methods: natives![
        "<init>" "(JJ)V" M => Some(|vm, a| uuid_op(vm, a, "init")),
        "randomUUID" "()Ljava/util/UUID;" S => Some(|vm, a| uuid_op(vm, a, "randomUUID")),
        "nameUUIDFromBytes" "([B)Ljava/util/UUID;" S => Some(|vm, a| uuid_op(vm, a, "nameUUIDFromBytes")),
        "fromString" "(Ljava/lang/String;)Ljava/util/UUID;" S => Some(|vm, a| uuid_op(vm, a, "fromString")),
        "getMostSignificantBits" "()J" M => Some(|vm, a| uuid_op(vm, a, "getMostSignificantBits")),
        "getLeastSignificantBits" "()J" M => Some(|vm, a| uuid_op(vm, a, "getLeastSignificantBits")),
        "version" "()I" M => Some(|vm, a| uuid_op(vm, a, "version")),
        "variant" "()I" M => Some(|vm, a| uuid_op(vm, a, "variant")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| uuid_op(vm, a, "toString")),
        "hashCode" "()I" M => Some(|vm, a| uuid_op(vm, a, "hashCode")),
        "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| uuid_op(vm, a, "equals")),
        "compareTo" "(Ljava/util/UUID;)I" M => Some(|vm, a| uuid_op(vm, a, "compareTo")),
        "compareTo" "(Ljava/lang/Object;)I" M => Some(|vm, a| uuid_op(vm, a, "compareTo")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/HexFormat", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC | FINAL, methods: natives![
        "of" "()Ljava/util/HexFormat;" S => Some(|vm, a| hex_op(vm, a, "of")),
        "ofDelimiter" "(Ljava/lang/String;)Ljava/util/HexFormat;" S => Some(|vm, a| hex_op(vm, a, "ofDelimiter")),
        "isHexDigit" "(I)Z" S => Some(|vm, a| hex_op(vm, a, "isHexDigit")),
        "fromHexDigit" "(I)I" S => Some(|vm, a| hex_op(vm, a, "fromHexDigit")),
        "fromHexDigits" "(Ljava/lang/CharSequence;)I" S => Some(|vm, a| hex_op(vm, a, "fromHexDigits")),
        "fromHexDigitsToLong" "(Ljava/lang/CharSequence;)J" S => Some(|vm, a| hex_op(vm, a, "fromHexDigitsToLong")),
        "withUpperCase" "()Ljava/util/HexFormat;" M => Some(|vm, a| hex_op(vm, a, "withUpperCase")),
        "withLowerCase" "()Ljava/util/HexFormat;" M => Some(|vm, a| hex_op(vm, a, "withLowerCase")),
        "withDelimiter" "(Ljava/lang/String;)Ljava/util/HexFormat;" M => Some(|vm, a| hex_op(vm, a, "withDelimiter")),
        "withPrefix" "(Ljava/lang/String;)Ljava/util/HexFormat;" M => Some(|vm, a| hex_op(vm, a, "withPrefix")),
        "withSuffix" "(Ljava/lang/String;)Ljava/util/HexFormat;" M => Some(|vm, a| hex_op(vm, a, "withSuffix")),
        "delimiter" "()Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "delimiter")),
        "prefix" "()Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "prefix")),
        "suffix" "()Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "suffix")),
        "isUpperCase" "()Z" M => Some(|vm, a| hex_op(vm, a, "isUpperCase")),
        "formatHex" "([B)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "formatHex")),
        "formatHex" "([BII)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "formatHex")),
        "parseHex" "(Ljava/lang/CharSequence;)[B" M => Some(|vm, a| hex_op(vm, a, "parseHex")),
        "toHexDigits" "(B)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toHexDigitsByte")),
        "toHexDigits" "(S)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toHexDigitsShort")),
        "toHexDigits" "(C)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toHexDigitsChar")),
        "toHexDigits" "(I)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toHexDigits")),
        "toHexDigits" "(J)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toHexDigits")),
        "toHexDigits" "(JI)Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toHexDigits")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| hex_op(vm, a, "toString")),
    ], statics: &[], clinit: None, data: None },
    jdk_enum!("java/lang/annotation/ElementType", ["TYPE", "FIELD", "METHOD", "PARAMETER", "CONSTRUCTOR", "LOCAL_VARIABLE", "ANNOTATION_TYPE", "PACKAGE", "TYPE_PARAMETER", "TYPE_USE", "MODULE", "RECORD_COMPONENT"]),
    jdk_enum!("java/lang/annotation/RetentionPolicy", ["SOURCE", "CLASS", "RUNTIME"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_zlib() {
        // CRC32 and Adler32 of "hello" as java.util.zip computes them.
        assert_eq!(checksum(0, b"hello", false), 0x3610_a686);
        assert_eq!(checksum(1, b"hello", true), 0x062c_0215);
    }

    #[test]
    fn splittable_random_matches_java() {
        // new SplittableRandom(99).nextInt() on HotSpot 25.
        let seed = 99u64.wrapping_add(GOLDEN_GAMMA);
        assert_eq!(mix32(seed), -801_679_754);
    }

    #[test]
    fn md5_and_name_uuids_match_java() {
        assert_eq!(md5(b"").iter().map(|b| format!("{b:02x}")).collect::<String>(), "d41d8cd98f00b204e9800998ecf8427e");
        // UUID.nameUUIDFromBytes("OfflinePlayer:Steve".getBytes()) on HotSpot.
        let mut b = md5(b"OfflinePlayer:Steve");
        b[6] = (b[6] & 0x0f) | 0x30;
        b[8] = (b[8] & 0x3f) | 0x80;
        let (m, l) = uuid_from_bytes(b);
        assert_eq!(uuid_text(m, l), "5627dd98-e6be-3c21-b8a8-e92344183641");
    }

    #[test]
    fn locale_hash_matches_hotspot() {
        assert_eq!(locale_hash("en", "US", ""), 3_118_343);
        assert_eq!(locale_hash("", "US", ""), 3742);
        assert_eq!(locale_hash("fr", "FR", ""), 3_151_512);
        assert_eq!(locale_hash("en", "US", "x"), 96_668_753);
    }

    #[test]
    fn properties_parse_their_format() {
        let units: Vec<u16> = "# comment\na=1\nb : two\\\n   lines\nc\\ d=\\u0041\n".encode_utf16().collect();
        let parsed: Vec<(String, String)> = parse_properties(&units).into_iter().map(|(k, v)| (String::from_utf16_lossy(&k), String::from_utf16_lossy(&v))).collect();
        assert_eq!(parsed, vec![("a".into(), "1".into()), ("b".into(), "twolines".into()), ("c d".into(), "A".into())]);
    }
}

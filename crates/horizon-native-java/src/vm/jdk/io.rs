//! `java.io` and charsets: print streams, byte and char streams in memory,
//! readers and writers. Standard output and error go to the VM's buffers;
//! nothing reaches a host file (docs/java-interpreter.md, "The sandbox").
use super::{A, INTERFACE, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, state_mut, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Array, Data, Exc, ObjId, Res, Value};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

// ------------------------------------------------------------------ charsets

/// A charset's canonical name, if the VM knows it.
pub fn canonical(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_uppercase().replace('_', "-");
    Some(match n.as_str() {
        "UTF-8" | "UTF8" => "UTF-8",
        "ISO-8859-1" | "ISO8859-1" | "LATIN1" | "ISO-LATIN-1" | "8859-1" | "CP819" | "L1" => "ISO-8859-1",
        "US-ASCII" | "ASCII" | "ASCII7" | "646" | "ISO646-US" | "DEFAULT" => "US-ASCII",
        "UTF-16" | "UTF16" => "UTF-16",
        "UTF-16BE" | "UNICODEBIGUNMARKED" | "X-UTF-16BE" => "UTF-16BE",
        "UTF-16LE" | "UNICODELITTLEUNMARKED" | "X-UTF-16LE" => "UTF-16LE",
        _ => return None,
    })
}

/// Bytes to UTF-16 as Java's decoders do: malformed input becomes U+FFFD.
pub fn decode(bytes: &[u8], charset: &str) -> Vec<u16> {
    match canonical(charset).unwrap_or("UTF-8") {
        "ISO-8859-1" => bytes.iter().map(|&b| b as u16).collect(),
        "US-ASCII" => bytes.iter().map(|&b| if b < 0x80 { b as u16 } else { 0xFFFD }).collect(),
        cs @ ("UTF-16" | "UTF-16BE" | "UTF-16LE") => {
            let (mut little, mut body) = (cs == "UTF-16LE", bytes);
            if cs == "UTF-16" && bytes.len() >= 2 {
                match (bytes[0], bytes[1]) {
                    (0xFE, 0xFF) => body = &bytes[2..],
                    (0xFF, 0xFE) => {
                        little = true;
                        body = &bytes[2..];
                    }
                    _ => {}
                }
            }
            let mut out: Vec<u16> = body
                .chunks_exact(2)
                .map(|p| if little { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) })
                .collect();
            if body.len() % 2 == 1 {
                out.push(0xFFFD);
            }
            out
        }
        _ => String::from_utf8_lossy(bytes).encode_utf16().collect(),
    }
}

/// UTF-16 to bytes as `String.getBytes` does: what a charset cannot hold becomes `?`.
pub fn encode(units: &[u16], charset: &str) -> Vec<u8> {
    match canonical(charset).unwrap_or("UTF-8") {
        "ISO-8859-1" => utf16_chars(units).map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' }).collect(),
        "US-ASCII" => utf16_chars(units).map(|c| if c.is_ascii() { c as u8 } else { b'?' }).collect(),
        "UTF-16" => {
            let mut out = vec![0xFE, 0xFF];
            out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
            out
        }
        "UTF-16BE" => units.iter().flat_map(|u| u.to_be_bytes()).collect(),
        "UTF-16LE" => units.iter().flat_map(|u| u.to_le_bytes()).collect(),
        _ => {
            let mut out = Vec::with_capacity(units.len());
            for c in utf16_chars(units) {
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
    }
}

/// UTF-16 units as chars, an unpaired surrogate as `?`.
fn utf16_chars(units: &[u16]) -> impl Iterator<Item = char> + '_ {
    char::decode_utf16(units.iter().copied()).map(|c| c.unwrap_or('?'))
}

/// The canonical name of a `Charset` object.
pub fn charset_name(vm: &Vm, r: ObjId) -> String {
    match state(vm, r) {
        Some(State::Generic(v)) if !v.is_empty() => vm.string(v[0].r()).unwrap_or_else(|| "UTF-8".into()),
        _ => "UTF-8".into(),
    }
}

fn charset_object(vm: &mut Vm, name: &str) -> Res<ObjId> {
    let c = vm.load("java/nio/charset/Charset")?;
    let r = vm.new_instance(c)?;
    let n = vm.intern(name)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(n)])));
    }
    Ok(r)
}

/// The charset an argument names (a `String` or a `Charset`), or the exception Java throws.
fn charset_arg(vm: &mut Vm, v: Value, string_exception: &str) -> Res<String> {
    let r = v.r();
    if r == 0 {
        return Err(vm.npe(Some("charset")));
    }
    if vm.string_units(r).is_some() {
        let name = vm.string(r).unwrap_or_default();
        return match canonical(&name) {
            Some(c) => Ok(c.to_string()),
            None => Err(vm.throw_new(string_exception, Some(&name))),
        };
    }
    Ok(charset_name(vm, r))
}

// ------------------------------------------------------------------ output

#[derive(Debug, Clone, Default)]
pub enum Sink {
    Stdout,
    Stderr,
    /// An `OutputStream` the bytes go to.
    Stream(ObjId),
    /// A `Writer` the chars go to.
    Writer(ObjId),
    /// The object's own buffer (`ByteArrayOutputStream`, `StringWriter`).
    #[default]
    Memory,
}

/// An output stream's or a writer's state.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub sink: Sink,
    pub charset: String,
    /// Chars a buffered writer holds until it flushes.
    pub pending: Vec<u16>,
    pub buffered: bool,
    pub autoflush: bool,
    pub bytes: Vec<u8>,
    pub chars: Vec<u16>,
    pub closed: bool,
    pub error: bool,
}

impl Out {
    fn new(sink: Sink) -> Out {
        Out {
            sink,
            charset: "UTF-8".into(),
            ..Default::default()
        }
    }

    pub fn refs(&self, f: &mut dyn FnMut(ObjId)) {
        match self.sink {
            Sink::Stream(r) | Sink::Writer(r) => f(r),
            _ => {}
        }
    }
}

fn out(vm: &Vm, r: ObjId) -> Option<&Out> {
    match state(vm, r) {
        Some(State::Out(o)) => Some(o),
        _ => None,
    }
}

fn out_mut(vm: &mut Vm, r: ObjId) -> Option<&mut Out> {
    match state_mut(vm, r) {
        Some(State::Out(o)) => Some(o),
        _ => None,
    }
}

fn set_out(vm: &mut Vm, r: ObjId, o: Out) {
    if let Some(obj) = vm.obj_mut(r) {
        obj.data = Data::Jdk(Box::new(State::Out(Box::new(o))));
    }
}

pub fn new_print_stream(vm: &mut Vm, sink: Sink) -> Res<ObjId> {
    let c = vm.load("java/io/PrintStream")?;
    let r = vm.new_instance(c)?;
    set_out(vm, r, Out::new(sink));
    vm.pinned.push(r);
    Ok(r)
}

/// Bytes written to an output stream object.
fn write_bytes(vm: &mut Vm, r: ObjId, bytes: &[u8]) -> Res<()> {
    let Some(o) = out(vm, r) else {
        // A program's own OutputStream: its write(byte[], int, int).
        let arr = byte_array(vm, bytes)?;
        vm.invoke_virtual(r, "write", "([BII)V", &[Value::Ref(arr), Value::Int(0), Value::Int(bytes.len() as i32)])?;
        return Ok(());
    };
    match o.sink.clone() {
        Sink::Stdout => vm.stdout.extend_from_slice(bytes),
        Sink::Stderr => vm.stderr.extend_from_slice(bytes),
        Sink::Memory => {
            if let Some(o) = out_mut(vm, r) {
                o.bytes.extend_from_slice(bytes);
            }
        }
        Sink::Stream(target) => write_bytes(vm, target, bytes)?,
        Sink::Writer(target) => {
            let units = decode(bytes, "UTF-8");
            write_chars(vm, target, &units)?;
        }
    }
    Ok(())
}

/// Chars written to a print stream or a writer object.
fn write_chars(vm: &mut Vm, r: ObjId, units: &[u16]) -> Res<()> {
    let Some(o) = out(vm, r) else {
        let arr = char_array(vm, units)?;
        vm.invoke_virtual(r, "write", "([CII)V", &[Value::Ref(arr), Value::Int(0), Value::Int(units.len() as i32)])?;
        return Ok(());
    };
    if o.closed && is_writer(vm, r) {
        return Err(vm.throw_new("java/io/IOException", Some("Stream closed")));
    }
    if o.buffered {
        if let Some(o) = out_mut(vm, r) {
            o.pending.extend_from_slice(units);
            if o.pending.len() < 8192 {
                return Ok(());
            }
        }
        return flush_pending(vm, r);
    }
    match o.sink.clone() {
        Sink::Memory if is_writer(vm, r) => {
            if let Some(o) = out_mut(vm, r) {
                o.chars.extend_from_slice(units);
            }
        }
        Sink::Writer(target) => write_chars(vm, target, units)?,
        _ => {
            let cs = o.charset.clone();
            let bytes = encode(units, &cs);
            write_bytes(vm, r, &bytes)?;
        }
    }
    Ok(())
}

/// A buffered writer's chars, out to where it writes.
fn flush_pending(vm: &mut Vm, r: ObjId) -> Res<()> {
    let Some(o) = out_mut(vm, r) else { return Ok(()) };
    let pending = std::mem::take(&mut o.pending);
    let (sink, cs) = (o.sink.clone(), o.charset.clone());
    if pending.is_empty() {
        return Ok(());
    }
    match sink {
        Sink::Writer(target) => write_chars(vm, target, &pending),
        Sink::Stream(target) => write_bytes(vm, target, &encode(&pending, &cs)),
        Sink::Stdout => {
            vm.stdout.extend(encode(&pending, &cs));
            Ok(())
        }
        Sink::Stderr => {
            vm.stderr.extend(encode(&pending, &cs));
            Ok(())
        }
        Sink::Memory => {
            if let Some(o) = out_mut(vm, r) {
                o.chars.extend(pending);
            }
            Ok(())
        }
    }
}

fn flush(vm: &mut Vm, r: ObjId) -> Res<()> {
    flush_pending(vm, r)?;
    match out(vm, r).map(|o| o.sink.clone()) {
        Some(Sink::Stream(t) | Sink::Writer(t)) => {
            if out(vm, t).is_some() {
                flush(vm, t)
            } else {
                vm.invoke_virtual(t, "flush", "()V", &[]).map(|_| ())
            }
        }
        _ => Ok(()),
    }
}

fn close(vm: &mut Vm, r: ObjId) -> Res<()> {
    if out(vm, r).is_some_and(|o| o.closed) {
        return Ok(());
    }
    flush(vm, r)?;
    let sink = out(vm, r).map(|o| o.sink.clone());
    if let Some(o) = out_mut(vm, r) {
        o.closed = true;
    }
    match sink {
        Some(Sink::Stream(t) | Sink::Writer(t)) => {
            if out(vm, t).is_some() {
                close(vm, t)
            } else {
                vm.invoke_virtual(t, "close", "()V", &[]).map(|_| ())
            }
        }
        _ => Ok(()),
    }
}

fn is_writer(vm: &Vm, r: ObjId) -> bool {
    let Some(mut c) = vm.class_of(r) else { return false };
    loop {
        let name = vm.class_name(c);
        if name == "java/io/Writer" {
            return true;
        }
        match vm.class(c).super_class {
            Some(s) => c = s,
            None => return false,
        }
    }
}

fn byte_array(vm: &mut Vm, bytes: &[u8]) -> Res<ObjId> {
    super::string::bytes_value(vm, bytes.to_vec()).map(|v| v.r())
}

fn char_array(vm: &mut Vm, units: &[u16]) -> Res<ObjId> {
    let r = vm.new_array_of("C", units.len() as i32)?;
    if let Some(Array::C(v)) = vm.array_mut(r) {
        v.copy_from_slice(units);
    }
    Ok(r)
}

/// `(array, offset, length)` checked as `Objects.checkFromIndexSize`.
fn range(vm: &mut Vm, len: usize, off: i32, n: i32) -> Res<(usize, usize)> {
    if off < 0 || n < 0 || off as i64 + n as i64 > len as i64 {
        let msg = format!("Range [{off}, {off} + {n}) out of bounds for length {len}");
        return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
    }
    Ok((off as usize, off as usize + n as usize))
}

/// `print`/`println` of a print stream or a print writer.
fn print(vm: &mut Vm, a: &[Value], desc: &str, newline: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let mut units = match (desc, a.get(1)) {
        ("", _) | (_, None) => Vec::new(),
        ("[C", Some(v)) => {
            if v.r() == 0 {
                return Err(vm.npe(None));
            }
            super::string::chars_of(vm, v.r(), None)?
        }
        (d, Some(v)) => vm.stringify(*v, d)?,
    };
    if newline {
        units.push('\n' as u16);
    }
    write_chars(vm, t, &units)?;
    if newline && out(vm, t).is_some_and(|o| o.autoflush) {
        flush(vm, t)?;
    }
    void()
}

fn printf(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    // printf(Locale, String, Object...) or printf(String, Object...).
    let (fmt, args) = match a {
        [_, l, f, args] if l.r() == 0 || vm.string_units(l.r()).is_none() => (f.r(), args.r()),
        [_, f, args, ..] => (f.r(), args.r()),
        _ => (0, 0),
    };
    let items = if args == 0 { Vec::new() } else { vm.ref_elements(args) };
    let units = super::string::format(vm, fmt, &items)?;
    write_chars(vm, t, &units)?;
    if out(vm, t).is_some_and(|o| o.autoflush) {
        flush(vm, t)?;
    }
    ret_ref(t)
}

fn stream_init(vm: &mut Vm, a: &[Value], writer: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let target = arg(a, 1).r();
    if target == 0 {
        return Err(vm.npe(None));
    }
    if vm.string_units(target).is_some() {
        let name = vm.string(target).unwrap_or_default();
        let msg = format!("{name} (Files are outside the sandbox)");
        return Err(vm.throw_new("java/io/FileNotFoundException", Some(&msg)));
    }
    let target_is_writer = is_writer(vm, target);
    let mut o = Out::new(if target_is_writer { Sink::Writer(target) } else { Sink::Stream(target) });
    o.autoflush = matches!(a.get(2), Some(Value::Int(1)));
    if let Some(cs) = a.get(3) {
        o.charset = charset_arg(vm, *cs, "java/io/UnsupportedEncodingException")?;
    }
    // A PrintWriter over a stream writes through a BufferedWriter.
    o.buffered = writer && !target_is_writer;
    set_out(vm, t, o);
    void()
}

fn print_methods_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "flush" => {
            flush(vm, t)?;
            void()
        }
        "close" => {
            close(vm, t)?;
            void()
        }
        "checkError" => {
            flush(vm, t)?;
            ret_bool(out(vm, t).is_some_and(|o| o.error))
        }
        "writeInt" => {
            if is_writer(vm, t) {
                write_chars(vm, t, &[arg(a, 1).i() as u16])?;
            } else {
                write_bytes(vm, t, &[arg(a, 1).i() as u8])?;
            }
            void()
        }
        "writeBytes" => {
            let b = super::string::bytes_of(vm, arg(a, 1).r())?;
            let (s, e) = if a.len() > 2 { range(vm, b.len(), arg(a, 2).i(), arg(a, 3).i())? } else { (0, b.len()) };
            write_bytes(vm, t, &b[s..e])?;
            void()
        }
        "writeChars" => {
            let c = super::string::chars_of(vm, arg(a, 1).r(), None)?;
            let (s, e) = if a.len() > 2 { range(vm, c.len(), arg(a, 2).i(), arg(a, 3).i())? } else { (0, c.len()) };
            write_chars(vm, t, &c[s..e])?;
            void()
        }
        "writeString" => {
            let c = super::string::units(vm, arg(a, 1).r())?;
            let (s, e) = if a.len() > 2 { range(vm, c.len(), arg(a, 2).i(), arg(a, 3).i())? } else { (0, c.len()) };
            write_chars(vm, t, &c[s..e])?;
            void()
        }
        "appendChar" => {
            write_chars(vm, t, &[arg(a, 1).i() as u16])?;
            ret_ref(t)
        }
        "append" => {
            let v = arg(a, 1);
            let mut units = if v.r() == 0 { "null".encode_utf16().collect() } else { vm.stringify(v, "Ljava/lang/Object;")? };
            if a.len() > 2 {
                let (s, e) = (arg(a, 2).i(), arg(a, 3).i());
                if s < 0 || e < s || e as usize > units.len() {
                    let msg = format!("start {s}, end {e}, length {}", units.len());
                    return Err(vm.throw_new("java/lang/IndexOutOfBoundsException", Some(&msg)));
                }
                units = units[s as usize..e as usize].to_vec();
            }
            write_chars(vm, t, &units)?;
            ret_ref(t)
        }
        "newLine" => {
            write_chars(vm, t, &['\n' as u16])?;
            void()
        }
        _ => Err(Exc::Unsupported(format!("PrintStream.{op}"))),
    }
}

fn memory_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let o = out(vm, t).cloned().unwrap_or_default();
    match op {
        "init" => {
            let n = arg(a, 1).i();
            if a.len() > 1 && n < 0 {
                let msg = format!("Negative initial size: {n}");
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some(&msg)));
            }
            set_out(vm, t, Out::new(Sink::Memory));
            void()
        }
        "toByteArray" => super::string::bytes_value(vm, o.bytes),
        "toString" => {
            if is_writer(vm, t) {
                return ret_ref(vm.new_string_units(o.chars)?);
            }
            let cs = if a.len() > 1 { charset_arg(vm, arg(a, 1), "java/io/UnsupportedEncodingException")? } else { "UTF-8".into() };
            ret_ref(vm.new_string_units(decode(&o.bytes, &cs))?)
        }
        "size" => Ok(Value::Int(if is_writer(vm, t) { o.chars.len() } else { o.bytes.len() } as i32)),
        "reset" => {
            if let Some(o) = out_mut(vm, t) {
                o.bytes.clear();
                o.chars.clear();
            }
            void()
        }
        "writeTo" => {
            let target = arg(a, 1).r();
            if is_writer(vm, t) {
                write_chars(vm, target, &o.chars)?;
            } else {
                write_bytes(vm, target, &o.bytes)?;
            }
            void()
        }
        "toCharArray" => ret_ref(char_array(vm, &o.chars)?),
        "getBuffer" => {
            let c = vm.load("java/lang/StringBuffer")?;
            let r = vm.new_instance(c)?;
            if let Some(obj) = vm.obj_mut(r) {
                obj.data = Data::Chars(o.chars);
            }
            ret_ref(r)
        }
        _ => Err(Exc::Unsupported(format!("memory stream {op}"))),
    }
}

fn writer_init(vm: &mut Vm, a: &[Value], buffered: bool) -> Res<Value> {
    let t = this(vm, a)?;
    let target = arg(a, 1).r();
    if target == 0 {
        return Err(vm.npe(None));
    }
    let mut o = Out::new(if is_writer(vm, target) { Sink::Writer(target) } else { Sink::Stream(target) });
    o.buffered = buffered || matches!(o.sink, Sink::Stream(_));
    if let Some(cs) = a.get(2).filter(|v| matches!(v, Value::Ref(_))) {
        o.charset = charset_arg(vm, *cs, "java/io/UnsupportedEncodingException")?;
    }
    set_out(vm, t, o);
    void()
}

// ------------------------------------------------------------------ input

/// An input stream's or a reader's state: bytes or chars in memory, read
/// from `from` when first needed.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub bytes: Vec<u8>,
    pub chars: Vec<u16>,
    pub pos: usize,
    pub mark: usize,
    pub from: ObjId,
    pub loaded: bool,
    pub charset: String,
    pub closed: bool,
}

impl Source {
    pub fn refs(&self, f: &mut dyn FnMut(ObjId)) {
        f(self.from);
    }
}

fn source(vm: &Vm, r: ObjId) -> Option<&Source> {
    match state(vm, r) {
        Some(State::In(s)) => Some(s),
        _ => None,
    }
}

fn source_mut(vm: &mut Vm, r: ObjId) -> Option<&mut Source> {
    match state_mut(vm, r) {
        Some(State::In(s)) => Some(s),
        _ => None,
    }
}

fn set_source(vm: &mut Vm, r: ObjId, s: Source) {
    if let Some(obj) = vm.obj_mut(r) {
        obj.data = Data::Jdk(Box::new(State::In(Box::new(s))));
    }
}

pub fn new_input_stream(vm: &mut Vm, bytes: Vec<u8>) -> Res<ObjId> {
    let c = vm.load("java/io/ByteArrayInputStream")?;
    let r = vm.new_instance(c)?;
    set_source(vm, r, Source { bytes, loaded: true, ..Default::default() });
    vm.pinned.push(r);
    Ok(r)
}

/// Every byte left in an input stream object.
fn read_all_bytes(vm: &mut Vm, r: ObjId) -> Res<Vec<u8>> {
    if source(vm, r).is_some() {
        load_bytes(vm, r)?;
        let Some(s) = source_mut(vm, r) else { return Ok(Vec::new()) };
        let rest = s.bytes[s.pos.min(s.bytes.len())..].to_vec();
        s.pos = s.bytes.len();
        return Ok(rest);
    }
    let mut out = Vec::new();
    let buf = vm.new_array_of("B", 8192)?;
    loop {
        let n = vm.invoke_virtual(r, "read", "([BII)I", &[Value::Ref(buf), Value::Int(0), Value::Int(8192)])?.i();
        if n < 0 {
            break;
        }
        if let Some(Array::B(b)) = vm.array(buf) {
            out.extend(b[..n as usize].iter().map(|&x| x as u8));
        }
    }
    Ok(out)
}

fn load_bytes(vm: &mut Vm, r: ObjId) -> Res<()> {
    let Some(s) = source(vm, r) else { return Ok(()) };
    if s.loaded {
        return Ok(());
    }
    let from = s.from;
    let bytes = if from != 0 { read_all_bytes(vm, from)? } else { Vec::new() };
    if let Some(s) = source_mut(vm, r) {
        s.bytes = bytes;
        s.loaded = true;
    }
    Ok(())
}

/// Every char a reader has left (a native reader's buffer, or a program's
/// reader read to its end).
fn load_chars(vm: &mut Vm, r: ObjId) -> Res<()> {
    let Some(s) = source(vm, r) else { return Ok(()) };
    if s.loaded {
        return Ok(());
    }
    let (from, cs) = (s.from, s.charset.clone());
    let chars = if from == 0 {
        Vec::new()
    } else if is_reader(vm, from) {
        reader_rest(vm, from)?
    } else {
        let bytes = read_all_bytes(vm, from)?;
        decode(&bytes, if cs.is_empty() { "UTF-8" } else { &cs })
    };
    if let Some(s) = source_mut(vm, r) {
        s.chars = chars;
        s.loaded = true;
    }
    Ok(())
}

fn reader_rest(vm: &mut Vm, r: ObjId) -> Res<Vec<u16>> {
    if source(vm, r).is_some() {
        load_chars(vm, r)?;
        let Some(s) = source_mut(vm, r) else { return Ok(Vec::new()) };
        let rest = s.chars[s.pos.min(s.chars.len())..].to_vec();
        s.pos = s.chars.len();
        return Ok(rest);
    }
    let mut out = Vec::new();
    let buf = vm.new_array_of("C", 8192)?;
    loop {
        let n = vm.invoke_virtual(r, "read", "([CII)I", &[Value::Ref(buf), Value::Int(0), Value::Int(8192)])?.i();
        if n < 0 {
            break;
        }
        if let Some(Array::C(b)) = vm.array(buf) {
            out.extend_from_slice(&b[..n as usize]);
        }
    }
    Ok(out)
}

fn is_reader(vm: &Vm, r: ObjId) -> bool {
    let Some(mut c) = vm.class_of(r) else { return false };
    loop {
        if vm.class_name(c) == "java/io/Reader" {
            return true;
        }
        match vm.class(c).super_class {
            Some(s) => c = s,
            None => return false,
        }
    }
}

fn input_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "initBytes" => {
            let arr = arg(a, 1).r();
            let b = super::string::bytes_of(vm, arr)?;
            let (s, e) = if a.len() > 2 {
                let (o, n) = (arg(a, 2).i().max(0) as usize, arg(a, 3).i().max(0) as usize);
                (o.min(b.len()), (o + n).min(b.len()))
            } else {
                (0, b.len())
            };
            set_source(vm, t, Source { bytes: b, pos: s, mark: s, loaded: true, ..Default::default() });
            if let Some(src) = source_mut(vm, t) {
                src.bytes.truncate(e);
            }
            void()
        }
        "initFrom" => {
            let from = arg(a, 1).r();
            if from == 0 {
                return Err(vm.npe(None));
            }
            let cs = match a.get(2) {
                Some(v @ Value::Ref(_)) => charset_arg(vm, *v, "java/io/UnsupportedEncodingException")?,
                _ => "UTF-8".into(),
            };
            set_source(vm, t, Source { from, charset: cs, ..Default::default() });
            void()
        }
        "initString" => {
            let s = super::string::units(vm, arg(a, 1).r())?;
            set_source(vm, t, Source { chars: s, loaded: true, ..Default::default() });
            void()
        }
        "read" => {
            load_bytes(vm, t)?;
            let Some(s) = source_mut(vm, t) else { return Ok(Value::Int(-1)) };
            match s.bytes.get(s.pos) {
                Some(&b) => {
                    s.pos += 1;
                    Ok(Value::Int(b as i32))
                }
                None => Ok(Value::Int(-1)),
            }
        }
        "readInto" => {
            load_bytes(vm, t)?;
            let arr = arg(a, 1).r();
            let len = vm.array(arr).map_or(0, Array::len);
            let (off, n) = if a.len() > 2 { (arg(a, 2).i(), arg(a, 3).i()) } else { (0, len as i32) };
            let (s, e) = range(vm, len, off, n)?;
            if s == e {
                return Ok(Value::Int(0));
            }
            let Some(src) = source_mut(vm, t) else { return Ok(Value::Int(-1)) };
            if src.pos >= src.bytes.len() {
                return Ok(Value::Int(-1));
            }
            let take = (e - s).min(src.bytes.len() - src.pos);
            let chunk: Vec<i8> = src.bytes[src.pos..src.pos + take].iter().map(|&b| b as i8).collect();
            src.pos += take;
            if let Some(Array::B(b)) = vm.array_mut(arr) {
                b[s..s + take].copy_from_slice(&chunk);
            }
            Ok(Value::Int(take as i32))
        }
        "readAllBytes" => {
            let b = read_all_bytes(vm, t)?;
            super::string::bytes_value(vm, b)
        }
        "readNBytes" => {
            load_bytes(vm, t)?;
            let n = arg(a, 1).i();
            if n < 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("len < 0")));
            }
            let Some(src) = source_mut(vm, t) else { return super::string::bytes_value(vm, Vec::new()) };
            let take = (n as usize).min(src.bytes.len().saturating_sub(src.pos));
            let chunk = src.bytes[src.pos..src.pos + take].to_vec();
            src.pos += take;
            super::string::bytes_value(vm, chunk)
        }
        "available" => {
            load_bytes(vm, t)?;
            Ok(Value::Int(source(vm, t).map_or(0, |s| s.bytes.len().saturating_sub(s.pos)) as i32))
        }
        "skip" => {
            load_bytes(vm, t)?;
            let n = arg(a, 1).j().max(0) as usize;
            let Some(src) = source_mut(vm, t) else { return Ok(Value::Long(0)) };
            let take = n.min(src.bytes.len().saturating_sub(src.pos));
            src.pos += take;
            Ok(Value::Long(take as i64))
        }
        "mark" => {
            if let Some(src) = source_mut(vm, t) {
                src.mark = src.pos;
            }
            void()
        }
        "reset" => {
            if let Some(src) = source_mut(vm, t) {
                src.pos = src.mark;
            }
            void()
        }
        "markSupported" => ret_bool(true),
        "close" => {
            if let Some(src) = source_mut(vm, t) {
                src.closed = true;
            }
            void()
        }
        "transferTo" => {
            let b = read_all_bytes(vm, t)?;
            write_bytes(vm, arg(a, 1).r(), &b)?;
            Ok(Value::Long(b.len() as i64))
        }
        _ => Err(Exc::Unsupported(format!("InputStream.{op}"))),
    }
}

fn reader_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    if source(vm, t).is_some_and(|s| s.closed) && op != "close" {
        return Err(vm.throw_new("java/io/IOException", Some("Stream closed")));
    }
    load_chars(vm, t)?;
    match op {
        "read" => {
            let Some(s) = source_mut(vm, t) else { return Ok(Value::Int(-1)) };
            match s.chars.get(s.pos) {
                Some(&c) => {
                    s.pos += 1;
                    Ok(Value::Int(c as i32))
                }
                None => Ok(Value::Int(-1)),
            }
        }
        "readInto" => {
            let arr = arg(a, 1).r();
            let len = vm.array(arr).map_or(0, Array::len);
            let (off, n) = if a.len() > 2 { (arg(a, 2).i(), arg(a, 3).i()) } else { (0, len as i32) };
            let (s, e) = range(vm, len, off, n)?;
            if s == e {
                return Ok(Value::Int(0));
            }
            let Some(src) = source_mut(vm, t) else { return Ok(Value::Int(-1)) };
            if src.pos >= src.chars.len() {
                return Ok(Value::Int(-1));
            }
            let take = (e - s).min(src.chars.len() - src.pos);
            let chunk = src.chars[src.pos..src.pos + take].to_vec();
            src.pos += take;
            if let Some(Array::C(b)) = vm.array_mut(arr) {
                b[s..s + take].copy_from_slice(&chunk);
            }
            Ok(Value::Int(take as i32))
        }
        "readLine" => {
            let Some(s) = source_mut(vm, t) else { return ret_ref(0) };
            if s.pos >= s.chars.len() {
                return ret_ref(0);
            }
            let start = s.pos;
            let mut end = start;
            while end < s.chars.len() && s.chars[end] != '\n' as u16 && s.chars[end] != '\r' as u16 {
                end += 1;
            }
            let line = s.chars[start..end].to_vec();
            s.pos = end;
            if s.pos < s.chars.len() && s.chars[s.pos] == '\r' as u16 {
                s.pos += 1;
            }
            if s.pos < s.chars.len() && s.chars[s.pos] == '\n' as u16 && (end == s.pos || s.chars[s.pos - 1] != '\n' as u16 || end + 1 == s.pos) {
                s.pos += 1;
            }
            ret_ref(vm.new_string_units(line)?)
        }
        "lines" => {
            let mut items = Vec::new();
            while let Value::Ref(l) = reader_op(vm, &[Value::Ref(t)], "readLine")? {
                if l == 0 {
                    break;
                }
                items.push(Value::Ref(l));
            }
            super::stream::stream_of(vm, items)
        }
        "ready" => ret_bool(source(vm, t).is_some_and(|s| s.pos < s.chars.len())),
        "skip" => {
            let n = arg(a, 1).j().max(0) as usize;
            let Some(src) = source_mut(vm, t) else { return Ok(Value::Long(0)) };
            let take = n.min(src.chars.len().saturating_sub(src.pos));
            src.pos += take;
            Ok(Value::Long(take as i64))
        }
        "mark" => {
            if let Some(src) = source_mut(vm, t) {
                src.mark = src.pos;
            }
            void()
        }
        "reset" => {
            if let Some(src) = source_mut(vm, t) {
                src.pos = src.mark;
            }
            void()
        }
        "markSupported" => ret_bool(true),
        "close" => {
            if let Some(src) = source_mut(vm, t) {
                src.closed = true;
            }
            void()
        }
        "transferTo" => {
            let rest = reader_rest(vm, t)?;
            write_chars(vm, arg(a, 1).r(), &rest)?;
            Ok(Value::Long(rest.len() as i64))
        }
        "readAll" => {
            let rest = reader_rest(vm, t)?;
            ret_ref(vm.new_string_units(rest)?)
        }
        _ => Err(Exc::Unsupported(format!("Reader.{op}"))),
    }
}

// ------------------------------------------------------------------ classes

macro_rules! print_family {
    ($f:ident) => {
        natives![
            "print" "(Z)V" M => Some(|vm, a| $f(vm, a, "Z", false)),
            "print" "(C)V" M => Some(|vm, a| $f(vm, a, "C", false)),
            "print" "(I)V" M => Some(|vm, a| $f(vm, a, "I", false)),
            "print" "(J)V" M => Some(|vm, a| $f(vm, a, "J", false)),
            "print" "(F)V" M => Some(|vm, a| $f(vm, a, "F", false)),
            "print" "(D)V" M => Some(|vm, a| $f(vm, a, "D", false)),
            "print" "([C)V" M => Some(|vm, a| $f(vm, a, "[C", false)),
            "print" "(Ljava/lang/String;)V" M => Some(|vm, a| $f(vm, a, "Ljava/lang/String;", false)),
            "print" "(Ljava/lang/Object;)V" M => Some(|vm, a| $f(vm, a, "Ljava/lang/Object;", false)),
            "println" "()V" M => Some(|vm, a| $f(vm, a, "", true)),
            "println" "(Z)V" M => Some(|vm, a| $f(vm, a, "Z", true)),
            "println" "(C)V" M => Some(|vm, a| $f(vm, a, "C", true)),
            "println" "(I)V" M => Some(|vm, a| $f(vm, a, "I", true)),
            "println" "(J)V" M => Some(|vm, a| $f(vm, a, "J", true)),
            "println" "(F)V" M => Some(|vm, a| $f(vm, a, "F", true)),
            "println" "(D)V" M => Some(|vm, a| $f(vm, a, "D", true)),
            "println" "([C)V" M => Some(|vm, a| $f(vm, a, "[C", true)),
            "println" "(Ljava/lang/String;)V" M => Some(|vm, a| $f(vm, a, "Ljava/lang/String;", true)),
            "println" "(Ljava/lang/Object;)V" M => Some(|vm, a| $f(vm, a, "Ljava/lang/Object;", true)),
        ]
    };
}

static PRINT_METHODS: &[NativeMethod] = print_family!(print);

static PRINT_STREAM_EXTRA: &[NativeMethod] = natives![
    "<init>" "(Ljava/io/OutputStream;)V" M => Some(|vm, a| stream_init(vm, a, false)),
    "<init>" "(Ljava/io/OutputStream;Z)V" M => Some(|vm, a| stream_init(vm, a, false)),
    "<init>" "(Ljava/io/OutputStream;ZLjava/lang/String;)V" M => Some(|vm, a| stream_init(vm, a, false)),
    "<init>" "(Ljava/io/OutputStream;ZLjava/nio/charset/Charset;)V" M => Some(|vm, a| stream_init(vm, a, false)),
    "<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| stream_init(vm, a, false)),
    "printf" "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;" M => Some(printf),
    "printf" "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;" M => Some(printf),
    "format" "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;" M => Some(printf),
    "format" "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;" M => Some(printf),
    "write" "(I)V" M => Some(|vm, a| print_methods_op(vm, a, "writeInt")),
    "write" "([B)V" M => Some(|vm, a| print_methods_op(vm, a, "writeBytes")),
    "write" "([BII)V" M => Some(|vm, a| print_methods_op(vm, a, "writeBytes")),
    "writeBytes" "([B)V" M => Some(|vm, a| print_methods_op(vm, a, "writeBytes")),
    "flush" "()V" M => Some(|vm, a| print_methods_op(vm, a, "flush")),
    "close" "()V" M => Some(|vm, a| print_methods_op(vm, a, "close")),
    "checkError" "()Z" M => Some(|vm, a| print_methods_op(vm, a, "checkError")),
    "append" "(Ljava/lang/CharSequence;)Ljava/io/PrintStream;" M => Some(|vm, a| print_methods_op(vm, a, "append")),
    "append" "(Ljava/lang/CharSequence;II)Ljava/io/PrintStream;" M => Some(|vm, a| print_methods_op(vm, a, "append")),
    "append" "(C)Ljava/io/PrintStream;" M => Some(|vm, a| print_methods_op(vm, a, "appendChar")),
];

static PRINT_WRITER_EXTRA: &[NativeMethod] = natives![
    "<init>" "(Ljava/io/Writer;)V" M => Some(|vm, a| stream_init(vm, a, true)),
    "<init>" "(Ljava/io/Writer;Z)V" M => Some(|vm, a| stream_init(vm, a, true)),
    "<init>" "(Ljava/io/OutputStream;)V" M => Some(|vm, a| stream_init(vm, a, true)),
    "<init>" "(Ljava/io/OutputStream;Z)V" M => Some(|vm, a| stream_init(vm, a, true)),
    "<init>" "(Ljava/io/OutputStream;ZLjava/nio/charset/Charset;)V" M => Some(|vm, a| stream_init(vm, a, true)),
    "<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| stream_init(vm, a, true)),
    "printf" "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintWriter;" M => Some(printf),
    "printf" "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintWriter;" M => Some(printf),
    "format" "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintWriter;" M => Some(printf),
    "format" "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintWriter;" M => Some(printf),
    "write" "(I)V" M => Some(|vm, a| print_methods_op(vm, a, "writeInt")),
    "write" "([C)V" M => Some(|vm, a| print_methods_op(vm, a, "writeChars")),
    "write" "([CII)V" M => Some(|vm, a| print_methods_op(vm, a, "writeChars")),
    "write" "(Ljava/lang/String;)V" M => Some(|vm, a| print_methods_op(vm, a, "writeString")),
    "write" "(Ljava/lang/String;II)V" M => Some(|vm, a| print_methods_op(vm, a, "writeString")),
    "flush" "()V" M => Some(|vm, a| print_methods_op(vm, a, "flush")),
    "close" "()V" M => Some(|vm, a| print_methods_op(vm, a, "close")),
    "checkError" "()Z" M => Some(|vm, a| print_methods_op(vm, a, "checkError")),
    "append" "(Ljava/lang/CharSequence;)Ljava/io/PrintWriter;" M => Some(|vm, a| print_methods_op(vm, a, "append")),
    "append" "(C)Ljava/io/PrintWriter;" M => Some(|vm, a| print_methods_op(vm, a, "appendChar")),
];

/// The writer methods every native writer shares.
static WRITER_METHODS: &[NativeMethod] = natives![
    "write" "(I)V" M => Some(|vm, a| print_methods_op(vm, a, "writeInt")),
    "write" "([C)V" M => Some(|vm, a| print_methods_op(vm, a, "writeChars")),
    "write" "([CII)V" M => Some(|vm, a| print_methods_op(vm, a, "writeChars")),
    "write" "(Ljava/lang/String;)V" M => Some(|vm, a| print_methods_op(vm, a, "writeString")),
    "write" "(Ljava/lang/String;II)V" M => Some(|vm, a| print_methods_op(vm, a, "writeString")),
    "append" "(Ljava/lang/CharSequence;)Ljava/io/Writer;" M => Some(|vm, a| print_methods_op(vm, a, "append")),
    "append" "(Ljava/lang/CharSequence;II)Ljava/io/Writer;" M => Some(|vm, a| print_methods_op(vm, a, "append")),
    "append" "(C)Ljava/io/Writer;" M => Some(|vm, a| print_methods_op(vm, a, "appendChar")),
    "flush" "()V" M => Some(|vm, a| print_methods_op(vm, a, "flush")),
    "close" "()V" M => Some(|vm, a| print_methods_op(vm, a, "close")),
    "newLine" "()V" M => Some(|vm, a| print_methods_op(vm, a, "newLine")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| memory_op(vm, a, "toString")),
    "getBuffer" "()Ljava/lang/StringBuffer;" M => Some(|vm, a| memory_op(vm, a, "getBuffer")),
    "toCharArray" "()[C" M => Some(|vm, a| memory_op(vm, a, "toCharArray")),
    "size" "()I" M => Some(|vm, a| memory_op(vm, a, "size")),
    "reset" "()V" M => Some(|vm, a| memory_op(vm, a, "reset")),
];

static READER_METHODS: &[NativeMethod] = natives![
    "read" "()I" M => Some(|vm, a| reader_op(vm, a, "read")),
    "read" "([C)I" M => Some(|vm, a| reader_op(vm, a, "readInto")),
    "read" "([CII)I" M => Some(|vm, a| reader_op(vm, a, "readInto")),
    "readLine" "()Ljava/lang/String;" M => Some(|vm, a| reader_op(vm, a, "readLine")),
    "lines" "()Ljava/util/stream/Stream;" M => Some(|vm, a| reader_op(vm, a, "lines")),
    "ready" "()Z" M => Some(|vm, a| reader_op(vm, a, "ready")),
    "skip" "(J)J" M => Some(|vm, a| reader_op(vm, a, "skip")),
    "mark" "(I)V" M => Some(|vm, a| reader_op(vm, a, "mark")),
    "reset" "()V" M => Some(|vm, a| reader_op(vm, a, "reset")),
    "markSupported" "()Z" M => Some(|vm, a| reader_op(vm, a, "markSupported")),
    "close" "()V" M => Some(|vm, a| reader_op(vm, a, "close")),
    "transferTo" "(Ljava/io/Writer;)J" M => Some(|vm, a| reader_op(vm, a, "transferTo")),
    "readAllAsString" "()Ljava/lang/String;" M => Some(|vm, a| reader_op(vm, a, "readAll")),
];

static INPUT_METHODS: &[NativeMethod] = natives![
    "read" "()I" M => Some(|vm, a| input_op(vm, a, "read")),
    "read" "([B)I" M => Some(|vm, a| input_op(vm, a, "readInto")),
    "read" "([BII)I" M => Some(|vm, a| input_op(vm, a, "readInto")),
    "readAllBytes" "()[B" M => Some(|vm, a| input_op(vm, a, "readAllBytes")),
    "readNBytes" "(I)[B" M => Some(|vm, a| input_op(vm, a, "readNBytes")),
    "available" "()I" M => Some(|vm, a| input_op(vm, a, "available")),
    "skip" "(J)J" M => Some(|vm, a| input_op(vm, a, "skip")),
    "mark" "(I)V" M => Some(|vm, a| input_op(vm, a, "mark")),
    "reset" "()V" M => Some(|vm, a| input_op(vm, a, "reset")),
    "markSupported" "()Z" M => Some(|vm, a| input_op(vm, a, "markSupported")),
    "close" "()V" M => Some(|vm, a| input_op(vm, a, "close")),
    "transferTo" "(Ljava/io/OutputStream;)J" M => Some(|vm, a| input_op(vm, a, "transferTo")),
];

/// `OutputStream.write(byte[], int, int)` for a program's stream: its write(int).
fn output_write_range(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let b = super::string::bytes_of(vm, arg(a, 1).r())?;
    let (s, e) = if a.len() > 2 { range(vm, b.len(), arg(a, 2).i(), arg(a, 3).i())? } else { (0, b.len()) };
    for &x in &b[s..e] {
        vm.invoke_virtual(t, "write", "(I)V", &[Value::Int(x as i8 as i32)])?;
    }
    void()
}

/// `InputStream.read(byte[], int, int)` for a program's stream: its read().
fn input_read_range(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let arr = arg(a, 1).r();
    let len = vm.array(arr).map_or(0, Array::len);
    let (off, n) = if a.len() > 2 { (arg(a, 2).i(), arg(a, 3).i()) } else { (0, len as i32) };
    let (s, e) = range(vm, len, off, n)?;
    if s == e {
        return Ok(Value::Int(0));
    }
    let mut i = s;
    while i < e {
        let c = vm.invoke_virtual(t, "read", "()I", &[])?.i();
        if c < 0 {
            break;
        }
        if let Some(arr) = vm.array_mut(arr) {
            arr.set(i, Value::Int(c as i8 as i32));
        }
        i += 1;
    }
    Ok(Value::Int(if i == s { -1 } else { (i - s) as i32 }))
}

/// `Reader.read()` for a program's reader: its read(char[], int, int).
fn reader_read_one(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let buf = vm.new_array_of("C", 1)?;
    let n = vm.invoke_virtual(t, "read", "([CII)I", &[Value::Ref(buf), Value::Int(0), Value::Int(1)])?.i();
    if n <= 0 {
        return Ok(Value::Int(-1));
    }
    Ok(vm.array(buf).and_then(|b| b.get(0)).unwrap_or(Value::Int(-1)))
}

fn writer_default(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let units: Vec<u16> = match op {
        "int" => vec![arg(a, 1).i() as u16],
        "chars" => super::string::chars_of(vm, arg(a, 1).r(), None)?,
        "string" => {
            let u = super::string::units(vm, arg(a, 1).r())?;
            if a.len() > 2 {
                let (s, e) = range(vm, u.len(), arg(a, 2).i(), arg(a, 3).i())?;
                u[s..e].to_vec()
            } else {
                u
            }
        }
        _ => {
            let v = arg(a, 1);
            if v.r() == 0 { "null".encode_utf16().collect() } else { vm.stringify(v, "Ljava/lang/Object;")? }
        }
    };
    let arr = char_array(vm, &units)?;
    vm.invoke_virtual(t, "write", "([CII)V", &[Value::Ref(arr), Value::Int(0), Value::Int(units.len() as i32)])?;
    if op == "append" { ret_ref(t) } else { void() }
}

const STANDARD_CHARSETS: &[(&str, &str)] = &[
    ("UTF_8", "UTF-8"),
    ("US_ASCII", "US-ASCII"),
    ("ISO_8859_1", "ISO-8859-1"),
    ("UTF_16", "UTF-16"),
    ("UTF_16BE", "UTF-16BE"),
    ("UTF_16LE", "UTF-16LE"),
];

static CLASSES: &[NativeClass] = &[
    class("java/io/Flushable", None, &[], INTERFACE, natives!["flush" "()V" A => None]),
    NativeClass { name: "java/io/OutputStream", super_name: Some("java/lang/Object"), interfaces: &["java/io/Closeable", "java/io/Flushable"], access: PUBLIC | 0x0400, methods: natives![
        "<init>" "()V" M => Some(|_, _| void()),
        "write" "(I)V" A => None,
        "write" "([B)V" M => Some(output_write_range),
        "write" "([BII)V" M => Some(output_write_range),
        "flush" "()V" M => Some(|_, _| void()),
        "close" "()V" M => Some(|_, _| void()),
        "nullOutputStream" "()Ljava/io/OutputStream;" S => Some(|vm, _| { let c = vm.load("java/io/ByteArrayOutputStream")?; let r = vm.new_instance(c)?; set_out(vm, r, Out::new(Sink::Memory)); ret_ref(r) }),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/FilterOutputStream", super_name: Some("java/io/OutputStream"), interfaces: &[], access: PUBLIC, methods: natives![
        "<init>" "(Ljava/io/OutputStream;)V" M => Some(|vm, a| stream_init(vm, a, false)),
        "write" "(I)V" M => Some(|vm, a| print_methods_op(vm, a, "writeInt")),
        "flush" "()V" M => Some(|vm, a| print_methods_op(vm, a, "flush")),
        "close" "()V" M => Some(|vm, a| print_methods_op(vm, a, "close")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/PrintStream", super_name: Some("java/io/FilterOutputStream"), interfaces: &["java/lang/Appendable"], access: PUBLIC, methods: PRINT_METHODS, statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Out(Box::new(Out::new(Sink::Memory)))))) },
    class("java/io/PrintStream$Extra", None, &[], 0, PRINT_STREAM_EXTRA),
    NativeClass { name: "java/io/BufferedOutputStream", super_name: Some("java/io/FilterOutputStream"), interfaces: &[], access: PUBLIC, methods: natives![
        "<init>" "(Ljava/io/OutputStream;)V" M => Some(|vm, a| stream_init(vm, a, false)),
        "<init>" "(Ljava/io/OutputStream;I)V" M => Some(|vm, a| stream_init(vm, &a[..2], false)),
        "write" "([BII)V" M => Some(|vm, a| print_methods_op(vm, a, "writeBytes")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/ByteArrayOutputStream", super_name: Some("java/io/OutputStream"), interfaces: &[], access: PUBLIC, methods: natives![
        "<init>" "()V" M => Some(|vm, a| memory_op(vm, a, "init")),
        "<init>" "(I)V" M => Some(|vm, a| memory_op(vm, a, "init")),
        "write" "(I)V" M => Some(|vm, a| print_methods_op(vm, a, "writeInt")),
        "write" "([BII)V" M => Some(|vm, a| print_methods_op(vm, a, "writeBytes")),
        "writeBytes" "([B)V" M => Some(|vm, a| print_methods_op(vm, a, "writeBytes")),
        "toByteArray" "()[B" M => Some(|vm, a| memory_op(vm, a, "toByteArray")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| memory_op(vm, a, "toString")),
        "toString" "(Ljava/lang/String;)Ljava/lang/String;" M => Some(|vm, a| memory_op(vm, a, "toString")),
        "toString" "(Ljava/nio/charset/Charset;)Ljava/lang/String;" M => Some(|vm, a| memory_op(vm, a, "toString")),
        "size" "()I" M => Some(|vm, a| memory_op(vm, a, "size")),
        "reset" "()V" M => Some(|vm, a| memory_op(vm, a, "reset")),
        "writeTo" "(Ljava/io/OutputStream;)V" M => Some(|vm, a| memory_op(vm, a, "writeTo")),
        "close" "()V" M => Some(|_, _| void()),
    ], statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Out(Box::new(Out::new(Sink::Memory)))))) },
    NativeClass { name: "java/io/Writer", super_name: Some("java/lang/Object"), interfaces: &["java/lang/Appendable", "java/io/Closeable", "java/io/Flushable"], access: PUBLIC | 0x0400, methods: natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "<init>" "(Ljava/lang/Object;)V" 0x0004 => Some(|_, _| void()),
        "write" "([CII)V" A => None,
        "write" "(I)V" M => Some(|vm, a| writer_default(vm, a, "int")),
        "write" "([C)V" M => Some(|vm, a| writer_default(vm, a, "chars")),
        "write" "(Ljava/lang/String;)V" M => Some(|vm, a| writer_default(vm, a, "string")),
        "write" "(Ljava/lang/String;II)V" M => Some(|vm, a| writer_default(vm, a, "string")),
        "append" "(Ljava/lang/CharSequence;)Ljava/io/Writer;" M => Some(|vm, a| writer_default(vm, a, "append")),
        "append" "(C)Ljava/io/Writer;" M => Some(|vm, a| { let c = arg(a, 1).i() as u16; let s = vm.new_string_units(vec![c])?; writer_default(vm, &[a[0], Value::Ref(s)], "append") }),
        "flush" "()V" A => None,
        "close" "()V" A => None,
        "nullWriter" "()Ljava/io/Writer;" S => Some(|vm, _| { let c = vm.load("java/io/StringWriter")?; let r = vm.new_instance(c)?; set_out(vm, r, Out::new(Sink::Memory)); ret_ref(r) }),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/StringWriter", super_name: Some("java/io/Writer"), interfaces: &[], access: PUBLIC, methods: WRITER_METHODS, statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Out(Box::new(Out::new(Sink::Memory)))))) },
    class("java/io/StringWriter$Extra", None, &[], 0, natives!["<init>" "()V" M => Some(|vm, a| memory_op(vm, a, "init")), "<init>" "(I)V" M => Some(|vm, a| memory_op(vm, a, "init"))]),
    NativeClass { name: "java/io/CharArrayWriter", super_name: Some("java/io/Writer"), interfaces: &[], access: PUBLIC, methods: WRITER_METHODS, statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Out(Box::new(Out::new(Sink::Memory)))))) },
    class("java/io/CharArrayWriter$Extra", None, &[], 0, natives!["<init>" "()V" M => Some(|vm, a| memory_op(vm, a, "init")), "<init>" "(I)V" M => Some(|vm, a| memory_op(vm, a, "init"))]),
    NativeClass { name: "java/io/PrintWriter", super_name: Some("java/io/Writer"), interfaces: &[], access: PUBLIC, methods: PRINT_METHODS, statics: &[], clinit: None, data: Some(|| Data::Jdk(Box::new(State::Out(Box::new(Out::new(Sink::Memory)))))) },
    class("java/io/PrintWriter$Extra", None, &[], 0, PRINT_WRITER_EXTRA),
    NativeClass { name: "java/io/BufferedWriter", super_name: Some("java/io/Writer"), interfaces: &[], access: PUBLIC, methods: WRITER_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/BufferedWriter$Extra", None, &[], 0, natives!["<init>" "(Ljava/io/Writer;)V" M => Some(|vm, a| writer_init(vm, a, true)), "<init>" "(Ljava/io/Writer;I)V" M => Some(|vm, a| writer_init(vm, &a[..2], true))]),
    NativeClass { name: "java/io/OutputStreamWriter", super_name: Some("java/io/Writer"), interfaces: &[], access: PUBLIC, methods: WRITER_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/OutputStreamWriter$Extra", None, &[], 0, natives![
        "<init>" "(Ljava/io/OutputStream;)V" M => Some(|vm, a| writer_init(vm, a, true)),
        "<init>" "(Ljava/io/OutputStream;Ljava/lang/String;)V" M => Some(|vm, a| writer_init(vm, a, true)),
        "<init>" "(Ljava/io/OutputStream;Ljava/nio/charset/Charset;)V" M => Some(|vm, a| writer_init(vm, a, true)),
    ]),
    NativeClass { name: "java/io/InputStream", super_name: Some("java/lang/Object"), interfaces: &["java/io/Closeable"], access: PUBLIC | 0x0400, methods: natives![
        "<init>" "()V" M => Some(|_, _| void()),
        "read" "()I" A => None,
        "read" "([B)I" M => Some(input_read_range),
        "read" "([BII)I" M => Some(input_read_range),
        "readAllBytes" "()[B" M => Some(|vm, a| { let t = this(vm, a)?; let b = read_all_bytes(vm, t)?; super::string::bytes_value(vm, b) }),
        "available" "()I" M => Some(|_, _| Ok(Value::Int(0))),
        "close" "()V" M => Some(|_, _| void()),
        "markSupported" "()Z" M => Some(|_, _| ret_bool(false)),
        "transferTo" "(Ljava/io/OutputStream;)J" M => Some(|vm, a| { let t = this(vm, a)?; let b = read_all_bytes(vm, t)?; write_bytes(vm, arg(a, 1).r(), &b)?; Ok(Value::Long(b.len() as i64)) }),
        "nullInputStream" "()Ljava/io/InputStream;" S => Some(|vm, _| ret_ref(new_input_stream(vm, Vec::new())?)),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/ByteArrayInputStream", super_name: Some("java/io/InputStream"), interfaces: &[], access: PUBLIC, methods: INPUT_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/ByteArrayInputStream$Extra", None, &[], 0, natives!["<init>" "([B)V" M => Some(|vm, a| input_op(vm, a, "initBytes")), "<init>" "([BII)V" M => Some(|vm, a| input_op(vm, a, "initBytes"))]),
    NativeClass { name: "java/io/FilterInputStream", super_name: Some("java/io/InputStream"), interfaces: &[], access: PUBLIC, methods: INPUT_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/FilterInputStream$Extra", None, &[], 0, natives!["<init>" "(Ljava/io/InputStream;)V" 0x0004 => Some(|vm, a| input_op(vm, a, "initFrom"))]),
    NativeClass { name: "java/io/BufferedInputStream", super_name: Some("java/io/FilterInputStream"), interfaces: &[], access: PUBLIC, methods: natives![
        "<init>" "(Ljava/io/InputStream;)V" M => Some(|vm, a| input_op(vm, a, "initFrom")),
        "<init>" "(Ljava/io/InputStream;I)V" M => Some(|vm, a| input_op(vm, &a[..2], "initFrom")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/Reader", super_name: Some("java/lang/Object"), interfaces: &["java/lang/Readable", "java/io/Closeable"], access: PUBLIC | 0x0400, methods: natives![
        "<init>" "()V" 0x0004 => Some(|_, _| void()),
        "read" "([CII)I" A => None,
        "read" "()I" M => Some(reader_read_one),
        "read" "([C)I" M => Some(|vm, a| { let t = this(vm, a)?; let n = vm.array(arg(a, 1).r()).map_or(0, Array::len); vm.invoke_virtual(t, "read", "([CII)I", &[arg(a, 1), Value::Int(0), Value::Int(n as i32)]) }),
        "close" "()V" A => None,
        "transferTo" "(Ljava/io/Writer;)J" M => Some(|vm, a| { let t = this(vm, a)?; let rest = reader_rest(vm, t)?; write_chars(vm, arg(a, 1).r(), &rest)?; Ok(Value::Long(rest.len() as i64)) }),
        "nullReader" "()Ljava/io/Reader;" S => Some(|vm, _| { let c = vm.load("java/io/StringReader")?; let r = vm.new_instance(c)?; set_source(vm, r, Source { loaded: true, ..Default::default() }); ret_ref(r) }),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/io/StringReader", super_name: Some("java/io/Reader"), interfaces: &[], access: PUBLIC, methods: READER_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/StringReader$Extra", None, &[], 0, natives!["<init>" "(Ljava/lang/String;)V" M => Some(|vm, a| input_op(vm, a, "initString"))]),
    NativeClass { name: "java/io/CharArrayReader", super_name: Some("java/io/Reader"), interfaces: &[], access: PUBLIC, methods: READER_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/CharArrayReader$Extra", None, &[], 0, natives!["<init>" "([C)V" M => Some(|vm, a| { let c = super::string::chars_of(vm, arg(a, 1).r(), None)?; let t = this(vm, a)?; set_source(vm, t, Source { chars: c, loaded: true, ..Default::default() }); void() })]),
    NativeClass { name: "java/io/InputStreamReader", super_name: Some("java/io/Reader"), interfaces: &[], access: PUBLIC, methods: READER_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/InputStreamReader$Extra", None, &[], 0, natives![
        "<init>" "(Ljava/io/InputStream;)V" M => Some(|vm, a| input_op(vm, a, "initFrom")),
        "<init>" "(Ljava/io/InputStream;Ljava/lang/String;)V" M => Some(|vm, a| input_op(vm, a, "initFrom")),
        "<init>" "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V" M => Some(|vm, a| input_op(vm, a, "initFrom")),
    ]),
    NativeClass { name: "java/io/BufferedReader", super_name: Some("java/io/Reader"), interfaces: &[], access: PUBLIC, methods: READER_METHODS, statics: &[], clinit: None, data: None },
    class("java/io/BufferedReader$Extra", None, &[], 0, natives![
        "<init>" "(Ljava/io/Reader;)V" M => Some(|vm, a| input_op(vm, a, "initFrom")),
        "<init>" "(Ljava/io/Reader;I)V" M => Some(|vm, a| input_op(vm, &a[..2], "initFrom")),
    ]),
    NativeClass { name: "java/nio/charset/Charset", super_name: Some("java/lang/Object"), interfaces: &["java/lang/Comparable"], access: PUBLIC | 0x0400, methods: natives![
        "forName" "(Ljava/lang/String;)Ljava/nio/charset/Charset;" S => Some(|vm, a| { let r = arg(a, 0).r(); if r == 0 { return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("Null charset name"))); } let name = vm.string(r).unwrap_or_default(); match canonical(&name) { Some(c) => ret_ref(charset_object(vm, c)?), None => Err(vm.throw_new("java/nio/charset/UnsupportedCharsetException", Some(&name))) } }),
        "defaultCharset" "()Ljava/nio/charset/Charset;" S => Some(|vm, _| ret_ref(charset_object(vm, "UTF-8")?)),
        "isSupported" "(Ljava/lang/String;)Z" S => Some(|vm, a| { let name = vm.string(arg(a, 0).r()).unwrap_or_default(); ret_bool(canonical(&name).is_some()) }),
        "name" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let n = charset_name(vm, t); ret_str(vm, &n) }),
        "displayName" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let n = charset_name(vm, t); ret_str(vm, &n) }),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| { let t = this(vm, a)?; let n = charset_name(vm, t); ret_str(vm, &n) }),
        "equals" "(Ljava/lang/Object;)Z" M => Some(|vm, a| { let t = this(vm, a)?; let o = arg(a, 1).r(); let same = vm.class_of(o) == vm.class_of(t) && charset_name(vm, o) == charset_name(vm, t); ret_bool(same) }),
        "hashCode" "()I" M => Some(|vm, a| { let t = this(vm, a)?; let n: Vec<u16> = charset_name(vm, t).encode_utf16().collect(); Ok(Value::Int(super::string::java_hash(&n))) }),
        "compareTo" "(Ljava/lang/Object;)I" M => Some(|vm, a| { let t = this(vm, a)?; let (x, y) = (charset_name(vm, t).to_lowercase(), charset_name(vm, arg(a, 1).r()).to_lowercase()); Ok(Value::Int(x.cmp(&y) as i32)) }),
    ], statics: &[], clinit: None, data: None },
    NativeClass {
        name: "java/nio/charset/StandardCharsets",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC,
        methods: &[],
        statics: &[
            ("UTF_8", "Ljava/nio/charset/Charset;"),
            ("US_ASCII", "Ljava/nio/charset/Charset;"),
            ("ISO_8859_1", "Ljava/nio/charset/Charset;"),
            ("UTF_16", "Ljava/nio/charset/Charset;"),
            ("UTF_16BE", "Ljava/nio/charset/Charset;"),
            ("UTF_16LE", "Ljava/nio/charset/Charset;"),
        ],
        clinit: Some(|vm, _| {
            let c = vm.load("java/nio/charset/StandardCharsets")?;
            for (field, name) in STANDARD_CHARSETS {
                let r = charset_object(vm, name)?;
                vm.pinned.push(r);
                vm.set_static(c, field, Value::Ref(r));
            }
            void()
        }),
        data: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charsets_replace_what_they_cannot_hold() {
        let units: Vec<u16> = "é€".encode_utf16().collect();
        assert_eq!(encode(&units, "ISO-8859-1"), vec![0xE9, b'?']);
        assert_eq!(encode(&units, "US-ASCII"), vec![b'?', b'?']);
        assert_eq!(encode(&[0xD800], "UTF-8"), vec![b'?']);
        assert_eq!(decode(&[0xFF, b'a'], "UTF-8"), vec![0xFFFD, b'a' as u16]);
        assert_eq!(decode(&[0xFF, 0xFE, b'a', 0], "UTF-16"), vec![b'a' as u16]);
    }
}

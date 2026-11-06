//! The class file format (JVMS 4), parsed completely and defensively for the
//! VM: every read is bounds-checked, every constant-pool index is checked
//! against its expected kind when it is used, and a malformed file is an
//! error, never a panic.
use std::sync::Arc;

/// A class file the parser refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError(pub String);

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, FormatError>;

fn bad<T>(what: impl Into<String>) -> Result<T> {
    Err(FormatError(what.into()))
}

/// A `CONSTANT_Utf8`: its text as a Rust string (lossy only for unpaired
/// surrogates, which no name contains) and as the UTF-16 a `String` holds.
#[derive(Debug, Clone)]
pub struct Utf8 {
    pub text: Arc<str>,
    pub units: Arc<[u16]>,
}

#[derive(Debug, Clone)]
pub enum Const {
    Unused,
    Utf8(Utf8),
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    Class(u16),
    Str(u16),
    Field { class: u16, nt: u16 },
    Method { class: u16, nt: u16 },
    IMethod { class: u16, nt: u16 },
    NameType { name: u16, desc: u16 },
    MethodHandle { kind: u8, reference: u16 },
    MethodType(u16),
    Dynamic { bsm: u16, nt: u16 },
    InvokeDynamic { bsm: u16, nt: u16 },
    Module(u16),
    Package(u16),
}

#[derive(Debug, Clone)]
pub struct Handler {
    pub start: u16,
    pub end: u16,
    pub handler: u16,
    /// Constant-pool class index; 0 catches everything (`finally`).
    pub catch_type: u16,
}

#[derive(Debug, Clone)]
pub struct Code {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Arc<[u8]>,
    pub handlers: Vec<Handler>,
    /// `(start_pc, line)`, sorted by `start_pc`.
    pub lines: Vec<(u16, u16)>,
    /// `(start_pc, length, name, slot)` from `LocalVariableTable`.
    pub locals: Vec<(u16, u16, Arc<str>, u16)>,
}

#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub access: u16,
    pub name: Arc<str>,
    pub desc: Arc<str>,
    pub signature: Option<Arc<str>>,
    pub code: Option<Arc<Code>>,
    /// `ConstantValue`'s pool index, for a static final field.
    pub constant: Option<u16>,
    pub annotations: Option<Arc<[u8]>>,
    pub parameter_annotations: Option<Arc<[u8]>>,
    pub annotation_default: Option<Arc<[u8]>>,
    pub exceptions: Vec<u16>,
    pub parameter_names: Vec<Option<Arc<str>>>,
}

#[derive(Debug, Clone)]
pub struct InnerClass {
    pub inner: u16,
    pub outer: u16,
    pub name: u16,
    pub access: u16,
}

#[derive(Debug, Clone)]
pub struct Bootstrap {
    pub method: u16,
    pub args: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct RecordComponent {
    pub name: Arc<str>,
    pub desc: Arc<str>,
    pub signature: Option<Arc<str>>,
}

#[derive(Debug, Clone)]
pub struct ClassFile {
    pub minor: u16,
    pub major: u16,
    pub pool: Vec<Const>,
    pub access: u16,
    pub name: Arc<str>,
    pub super_name: Option<Arc<str>>,
    pub interfaces: Vec<Arc<str>>,
    pub fields: Vec<MemberInfo>,
    pub methods: Vec<MemberInfo>,
    pub source_file: Option<Arc<str>>,
    pub signature: Option<Arc<str>>,
    pub inner_classes: Vec<InnerClass>,
    /// `EnclosingMethod`: the class index and the method's name-and-type (0 when none).
    pub enclosing: Option<(u16, u16)>,
    pub nest_host: Option<u16>,
    pub nest_members: Vec<u16>,
    pub permitted: Vec<u16>,
    pub record: Option<Vec<RecordComponent>>,
    pub bootstraps: Vec<Bootstrap>,
    pub annotations: Option<Arc<[u8]>>,
}

/// Bounds the parser holds a file to: no real class comes near them, and they
/// keep a crafted one from making it allocate without limit.
const MAX_POOL: usize = 65_535;
const MAX_MEMBERS: usize = 65_535;
const MAX_CODE: usize = 65_535;

pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&end| end <= self.bytes.len());
        match end {
            Some(end) => {
                let slice = &self.bytes[self.at..end];
                self.at = end;
                Ok(slice)
            }
            None => bad("truncated class file"),
        }
    }
    pub fn u1(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u2(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub fn u4(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }
}

/// Modified UTF-8 (JVMS 4.4.7) to UTF-16.
pub fn decode_modified_utf8(bytes: &[u8]) -> Result<Vec<u16>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let a = bytes[i] as u16;
        if a & 0x80 == 0 {
            if a == 0 {
                return bad("NUL byte in a Utf8 constant");
            }
            out.push(a);
            i += 1;
        } else if a & 0xE0 == 0xC0 {
            let b = *bytes.get(i + 1).ok_or_else(|| FormatError("bad Utf8".into()))? as u16;
            if b & 0xC0 != 0x80 {
                return bad("bad Utf8 continuation");
            }
            out.push(((a & 0x1F) << 6) | (b & 0x3F));
            i += 2;
        } else if a & 0xF0 == 0xE0 {
            let (Some(&b), Some(&c)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                return bad("bad Utf8");
            };
            if b & 0xC0 != 0x80 || c & 0xC0 != 0x80 {
                return bad("bad Utf8 continuation");
            }
            out.push(((a & 0x0F) << 12) | ((b as u16 & 0x3F) << 6) | (c as u16 & 0x3F));
            i += 3;
        } else {
            return bad("bad Utf8 lead byte");
        }
    }
    Ok(out)
}

impl ClassFile {
    pub fn parse(bytes: &[u8]) -> Result<ClassFile> {
        let mut r = Reader::new(bytes);
        if r.u4()? != 0xCAFE_BABE {
            return bad("not a class file (bad magic)");
        }
        let minor = r.u2()?;
        let major = r.u2()?;
        if !(45..=69).contains(&major) {
            return bad(format!("unsupported class file version {major}.{minor}"));
        }
        let count = r.u2()? as usize;
        if count == 0 || count > MAX_POOL {
            return bad("bad constant pool count");
        }
        let mut pool = Vec::with_capacity(count);
        pool.push(Const::Unused);
        while pool.len() < count {
            let tag = r.u1()?;
            let entry = match tag {
                1 => {
                    let len = r.u2()? as usize;
                    let units = decode_modified_utf8(r.take(len)?)?;
                    Const::Utf8(Utf8 {
                        text: String::from_utf16_lossy(&units).into(),
                        units: units.into(),
                    })
                }
                3 => Const::Int(r.u4()? as i32),
                4 => Const::Float(f32::from_bits(r.u4()?)),
                5 | 6 => {
                    let hi = r.u4()? as u64;
                    let lo = r.u4()? as u64;
                    let bits = (hi << 32) | lo;
                    let value = if tag == 5 {
                        Const::Long(bits as i64)
                    } else {
                        Const::Double(f64::from_bits(bits))
                    };
                    pool.push(value);
                    // Eight-byte constants take two entries.
                    Const::Unused
                }
                7 => Const::Class(r.u2()?),
                8 => Const::Str(r.u2()?),
                9 => Const::Field { class: r.u2()?, nt: r.u2()? },
                10 => Const::Method { class: r.u2()?, nt: r.u2()? },
                11 => Const::IMethod { class: r.u2()?, nt: r.u2()? },
                12 => Const::NameType { name: r.u2()?, desc: r.u2()? },
                15 => Const::MethodHandle { kind: r.u1()?, reference: r.u2()? },
                16 => Const::MethodType(r.u2()?),
                17 => Const::Dynamic { bsm: r.u2()?, nt: r.u2()? },
                18 => Const::InvokeDynamic { bsm: r.u2()?, nt: r.u2()? },
                19 => Const::Module(r.u2()?),
                20 => Const::Package(r.u2()?),
                other => return bad(format!("bad constant pool tag {other}")),
            };
            pool.push(entry);
        }
        if pool.len() > count {
            return bad("an eight-byte constant overruns the pool");
        }
        let mut cf = ClassFile {
            minor,
            major,
            pool,
            access: 0,
            name: "".into(),
            super_name: None,
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            source_file: None,
            signature: None,
            inner_classes: Vec::new(),
            enclosing: None,
            nest_host: None,
            nest_members: Vec::new(),
            permitted: Vec::new(),
            record: None,
            bootstraps: Vec::new(),
            annotations: None,
        };
        cf.access = r.u2()?;
        let this = r.u2()?;
        cf.name = cf.class_name(this)?;
        let sup = r.u2()?;
        cf.super_name = if sup == 0 { None } else { Some(cf.class_name(sup)?) };
        let n = r.u2()?;
        for _ in 0..n {
            let index = r.u2()?;
            let name = cf.class_name(index)?;
            cf.interfaces.push(name);
        }
        cf.fields = cf.members(&mut r, false)?;
        cf.methods = cf.members(&mut r, true)?;
        let n = r.u2()?;
        for _ in 0..n {
            let name = r.u2()?;
            let len = r.u4()? as usize;
            let body = r.take(len)?;
            let name = cf.utf8(name)?.text.clone();
            cf.class_attribute(&name, body)?;
        }
        if r.remaining() != 0 {
            return bad("trailing bytes after the class file");
        }
        Ok(cf)
    }

    fn members(&self, r: &mut Reader<'_>, methods: bool) -> Result<Vec<MemberInfo>> {
        let n = r.u2()? as usize;
        if n > MAX_MEMBERS {
            return bad("too many members");
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let access = r.u2()?;
            let name = self.utf8(r.u2()?)?.text.clone();
            let desc = self.utf8(r.u2()?)?.text.clone();
            let mut member = MemberInfo {
                access,
                name,
                desc,
                signature: None,
                code: None,
                constant: None,
                annotations: None,
                parameter_annotations: None,
                annotation_default: None,
                exceptions: Vec::new(),
                parameter_names: Vec::new(),
            };
            let attrs = r.u2()?;
            for _ in 0..attrs {
                let name = r.u2()?;
                let len = r.u4()? as usize;
                let body = r.take(len)?;
                let name = self.utf8(name)?.text.clone();
                let mut b = Reader::new(body);
                match &*name {
                    "Code" if methods => member.code = Some(Arc::new(self.code(body)?)),
                    "ConstantValue" if !methods => member.constant = Some(b.u2()?),
                    "Signature" => member.signature = Some(self.utf8(b.u2()?)?.text.clone()),
                    "RuntimeVisibleAnnotations" => member.annotations = Some(body.into()),
                    "RuntimeVisibleParameterAnnotations" => {
                        member.parameter_annotations = Some(body.into())
                    }
                    "AnnotationDefault" => member.annotation_default = Some(body.into()),
                    "Exceptions" => {
                        let n = b.u2()?;
                        for _ in 0..n {
                            member.exceptions.push(b.u2()?);
                        }
                    }
                    "MethodParameters" => {
                        let n = b.u1()?;
                        for _ in 0..n {
                            let name = b.u2()?;
                            let _flags = b.u2()?;
                            member.parameter_names.push(if name == 0 {
                                None
                            } else {
                                Some(self.utf8(name)?.text.clone())
                            });
                        }
                    }
                    _ => {}
                }
            }
            out.push(member);
        }
        Ok(out)
    }

    fn code(&self, body: &[u8]) -> Result<Code> {
        let mut r = Reader::new(body);
        let max_stack = r.u2()?;
        let max_locals = r.u2()?;
        let len = r.u4()? as usize;
        if len == 0 || len > MAX_CODE {
            return bad("bad code length");
        }
        let code: Arc<[u8]> = r.take(len)?.into();
        let n = r.u2()?;
        let mut handlers = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let h = Handler {
                start: r.u2()?,
                end: r.u2()?,
                handler: r.u2()?,
                catch_type: r.u2()?,
            };
            if h.start >= h.end || h.end as usize > len || h.handler as usize >= len {
                return bad("bad exception table entry");
            }
            handlers.push(h);
        }
        let mut lines = Vec::new();
        let mut locals = Vec::new();
        let n = r.u2()?;
        for _ in 0..n {
            let name = r.u2()?;
            let alen = r.u4()? as usize;
            let attr = r.take(alen)?;
            let name = self.utf8(name)?.text.clone();
            let mut a = Reader::new(attr);
            match &*name {
                "LineNumberTable" => {
                    let n = a.u2()?;
                    for _ in 0..n {
                        lines.push((a.u2()?, a.u2()?));
                    }
                }
                "LocalVariableTable" => {
                    let n = a.u2()?;
                    for _ in 0..n {
                        let start = a.u2()?;
                        let length = a.u2()?;
                        let name = self.utf8(a.u2()?)?.text.clone();
                        let _desc = a.u2()?;
                        let slot = a.u2()?;
                        locals.push((start, length, name, slot));
                    }
                }
                _ => {}
            }
        }
        lines.sort_by_key(|&(pc, _)| pc);
        Ok(Code {
            max_stack,
            max_locals,
            code,
            handlers,
            lines,
            locals,
        })
    }

    fn class_attribute(&mut self, name: &str, body: &[u8]) -> Result<()> {
        let mut r = Reader::new(body);
        match name {
            "SourceFile" => self.source_file = Some(self.utf8(r.u2()?)?.text.clone()),
            "Signature" => self.signature = Some(self.utf8(r.u2()?)?.text.clone()),
            "InnerClasses" => {
                let n = r.u2()?;
                for _ in 0..n {
                    self.inner_classes.push(InnerClass {
                        inner: r.u2()?,
                        outer: r.u2()?,
                        name: r.u2()?,
                        access: r.u2()?,
                    });
                }
            }
            "EnclosingMethod" => self.enclosing = Some((r.u2()?, r.u2()?)),
            "NestHost" => self.nest_host = Some(r.u2()?),
            "NestMembers" => {
                let n = r.u2()?;
                for _ in 0..n {
                    self.nest_members.push(r.u2()?);
                }
            }
            "PermittedSubclasses" => {
                let n = r.u2()?;
                for _ in 0..n {
                    self.permitted.push(r.u2()?);
                }
            }
            "BootstrapMethods" => {
                let n = r.u2()?;
                for _ in 0..n {
                    let method = r.u2()?;
                    let count = r.u2()?;
                    let mut args = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        args.push(r.u2()?);
                    }
                    self.bootstraps.push(Bootstrap { method, args });
                }
            }
            "Record" => {
                let n = r.u2()?;
                let mut components = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let name = self.utf8(r.u2()?)?.text.clone();
                    let desc = self.utf8(r.u2()?)?.text.clone();
                    let mut signature = None;
                    let attrs = r.u2()?;
                    for _ in 0..attrs {
                        let an = r.u2()?;
                        let len = r.u4()? as usize;
                        let ab = r.take(len)?;
                        if &*self.utf8(an)?.text == "Signature" {
                            signature = Some(self.utf8(Reader::new(ab).u2()?)?.text.clone());
                        }
                    }
                    components.push(RecordComponent {
                        name,
                        desc,
                        signature,
                    });
                }
                self.record = Some(components);
            }
            "RuntimeVisibleAnnotations" => self.annotations = Some(body.into()),
            _ => {}
        }
        Ok(())
    }

    pub fn get(&self, index: u16) -> Result<&Const> {
        self.pool
            .get(index as usize)
            .ok_or_else(|| FormatError(format!("constant pool index {index} out of range")))
    }

    pub fn utf8(&self, index: u16) -> Result<&Utf8> {
        match self.get(index)? {
            Const::Utf8(u) => Ok(u),
            other => bad(format!("constant {index} is {other:?}, not Utf8")),
        }
    }

    pub fn class_name(&self, index: u16) -> Result<Arc<str>> {
        match self.get(index)? {
            Const::Class(name) => Ok(self.utf8(*name)?.text.clone()),
            other => bad(format!("constant {index} is {other:?}, not a class")),
        }
    }

    pub fn name_and_type(&self, index: u16) -> Result<(Arc<str>, Arc<str>)> {
        match self.get(index)? {
            Const::NameType { name, desc } => Ok((
                self.utf8(*name)?.text.clone(),
                self.utf8(*desc)?.text.clone(),
            )),
            other => bad(format!("constant {index} is {other:?}, not a name and type")),
        }
    }

    /// A field or method reference: its class, name and descriptor, and
    /// whether it was an interface method reference.
    pub fn member_ref(&self, index: u16) -> Result<(Arc<str>, Arc<str>, Arc<str>, bool)> {
        let (class, nt, interface) = match self.get(index)? {
            Const::Field { class, nt } | Const::Method { class, nt } => (*class, *nt, false),
            Const::IMethod { class, nt } => (*class, *nt, true),
            other => return bad(format!("constant {index} is {other:?}, not a member reference")),
        };
        let class = self.class_name(class)?;
        let (name, desc) = self.name_and_type(nt)?;
        Ok((class, name, desc, interface))
    }

    pub fn line_of(code: &Code, pc: usize) -> Option<u16> {
        code.lines
            .iter()
            .take_while(|&&(start, _)| start as usize <= pc)
            .last()
            .map(|&(_, line)| line)
    }
}

/// The parameter types of a method descriptor, and its return type, as
/// descriptor strings (`I`, `[J`, `Ljava/lang/String;`).
pub fn parse_method_desc(desc: &str) -> Option<(Vec<&str>, &str)> {
    let rest = desc.strip_prefix('(')?;
    let close = rest.find(')')?;
    let (params, ret) = (&rest[..close], &rest[close + 1..]);
    let mut out = Vec::new();
    let mut i = 0;
    let b = params.as_bytes();
    while i < b.len() {
        let start = i;
        while b.get(i) == Some(&b'[') {
            i += 1;
        }
        match b.get(i)? {
            b'L' => {
                let end = params[i..].find(';')? + i;
                i = end + 1;
            }
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => i += 1,
            _ => return None,
        }
        out.push(&params[start..i]);
    }
    if ret.is_empty() {
        return None;
    }
    Some((out, ret))
}

/// Local slots a descriptor's parameters take (longs and doubles take two).
pub fn arg_slots(params: &[&str]) -> usize {
    params
        .iter()
        .map(|p| if *p == "J" || *p == "D" { 2 } else { 1 })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_descriptors_split_into_parameters() {
        assert_eq!(
            parse_method_desc("(I[JLjava/lang/String;[[Ljava/lang/Object;D)V"),
            Some((
                vec!["I", "[J", "Ljava/lang/String;", "[[Ljava/lang/Object;", "D"],
                "V"
            ))
        );
        assert_eq!(parse_method_desc("()Ljava/lang/String;"), Some((vec![], "Ljava/lang/String;")));
        assert_eq!(parse_method_desc("(Q)V"), None);
        assert_eq!(parse_method_desc("(I"), None);
    }

    #[test]
    fn modified_utf8_decodes_nul_and_surrogates() {
        // U+0000 as C0 80, U+00E9, U+1F600 as a surrogate pair of 3-byte forms.
        let bytes = [0xC0, 0x80, 0xC3, 0xA9, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80];
        assert_eq!(decode_modified_utf8(&bytes).unwrap(), vec![0, 0xE9, 0xD83D, 0xDE00]);
        assert!(decode_modified_utf8(&[0x00]).is_err());
        assert!(decode_modified_utf8(&[0xE0, 0x80]).is_err());
    }

    #[test]
    fn truncated_and_garbage_files_are_errors() {
        assert!(ClassFile::parse(&[]).is_err());
        assert!(ClassFile::parse(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]).is_err());
        let mut state = 0x1234_5678u32;
        for len in 0..200 {
            let mut bytes = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                bytes.push(state as u8);
            }
            let _ = ClassFile::parse(&bytes);
        }
    }
}

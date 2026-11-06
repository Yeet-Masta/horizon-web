//! Branch, exception-table and stack-map relocation tests for the weaver.
//!
//! The fixtures come from `javac`, so the weaver is exercised against real
//! `StackMapTable` frames, exception tables and debug tables.  Each woven class
//! is parsed back and every offset a rewrite touches is compared with the
//! original: branch targets, exception ranges and frame offsets must all move
//! by the insert delta and still name an instruction boundary.
//!
//! When a JDK is present the rewritten class is also loaded in a fresh
//! `URLClassLoader` and invoked: the verifier is the real judge of a relocated
//! frame, and the callback's counter proves the insert ran.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, ReturnMode, weave_class, weave_head, weave_return,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = r#"package demo;
public class Target {
    public static int total;

    public static int classify(int x) {
        if (x > 0) {
            return 1;
        } else {
            return -1;
        }
    }

    public static int loop(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += i;
        }
        return sum;
    }

    public static int guarded(int x) {
        try {
            return 100 / x;
        } catch (ArithmeticException e) {
            return -1;
        }
    }

    public static int multi(int x) {
        if (x < 0) {
            return -x;
        }
        if (x == 0) {
            return 1;
        }
        return x * 2;
    }

    public static void accumulate(int n) {
        total = 0;
        for (int i = 0; i <= n; i++) {
            total += i;
        }
        if (n > 10) {
            return;
        }
        total += 1000;
    }

    public static int switchy(int x) {
        switch (x) {
            case 0: return 10;
            case 1: return 11;
            case 2: return 12;
            default: return -1;
        }
    }
}"#;

const CALLBACK: &str = r#"package demo;
public final class Callback {
    private static int calls;

    public static int calls() { return calls; }
    public static void classify(int x) { calls++; }
    public static void loop(int n) { calls++; }
    public static void guarded(int x) { calls++; }
    public static void multi(int x) { calls++; }
    public static void accumulate(int n) { calls++; }
}"#;

/// A method whose 300 locals push the load operands past 255, so `javac`
/// emits `wide`.
fn wide_source() -> String {
    let declarations = (0..300)
        .map(|index| format!("        int v{index} = {index};"))
        .collect::<Vec<_>>()
        .join("\n");
    let sum = (0..300)
        .map(|index| format!("v{index}"))
        .collect::<Vec<_>>()
        .join(" + ");
    format!(
        "package demo;\npublic class Wide {{\n    public static int wide() {{\n{declarations}\n        return {sum};\n    }}\n}}\n"
    )
}

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn java() -> Option<PathBuf> {
    let path = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/java"))
        .unwrap_or_else(|| "java".into());
    Command::new(&path).arg("-version").output().ok()?;
    Some(path)
}

/// Compile `Target`, `Callback` and `Wide` into a fresh directory.
fn compile(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-frames-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src/demo");
    let classes = root.join("classes");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&classes).unwrap();
    for (file, body) in [
        ("Target.java", TARGET.to_owned()),
        ("Callback.java", CALLBACK.to_owned()),
        ("Wide.java", wide_source()),
    ] {
        fs::write(src.join(file), body).unwrap();
    }
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .arg(src.join("Target.java"))
        .arg(src.join("Callback.java"))
        .arg(src.join("Wide.java"))
        .status()
        .unwrap();
    assert!(status.success(), "javac failed for {name}");
    classes
}

fn read_class(classes: &Path, name: &str) -> Vec<u8> {
    fs::read(classes.join(format!("{name}.class"))).unwrap()
}

fn method(owner: &str, name: &str, descriptor: &str) -> MethodRef {
    MethodRef {
        owner: owner.into(),
        name: name.into(),
        descriptor: descriptor.into(),
    }
}

fn callback(name: &str, descriptor: &str) -> MethodRef {
    method("demo/Callback", name, descriptor)
}

fn u2(bytes: &[u8], at: &mut usize) -> u16 {
    let value = u16::from_be_bytes([bytes[*at], bytes[*at + 1]]);
    *at += 2;
    value
}

fn u4(bytes: &[u8], at: &mut usize) -> u32 {
    let value = u32::from_be_bytes([bytes[*at], bytes[*at + 1], bytes[*at + 2], bytes[*at + 3]]);
    *at += 4;
    value
}

/// A `Code` attribute read back with the offsets a rewrite touches.
struct CodeView {
    code: Vec<u8>,
    max_stack: u16,
    max_locals: u16,
    /// `(start_pc, end_pc, handler_pc)` per exception-table entry.
    handlers: Vec<(u16, u16, u16)>,
    /// Absolute bytecode offset per `StackMapTable` frame.
    frames: Vec<u32>,
}

fn read_pool(bytes: &[u8], at: &mut usize) -> Vec<Option<String>> {
    let count = u2(bytes, at) as usize;
    let mut pool = vec![None; count.max(1)];
    let mut index = 1;
    while index < count {
        let tag = bytes[*at];
        *at += 1;
        match tag {
            1 => {
                let length = u2(bytes, at) as usize;
                pool[index] = Some(String::from_utf8_lossy(&bytes[*at..*at + length]).into_owned());
                *at += length;
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => *at += 4,
            5 | 6 => {
                *at += 8;
                index += 1;
            }
            7 | 8 | 16 | 19 | 20 => *at += 2,
            15 => *at += 3,
            other => panic!("unexpected constant pool tag {other}"),
        }
        index += 1;
    }
    pool
}

fn skip_attributes(bytes: &[u8], at: &mut usize) {
    let count = u2(bytes, at) as usize;
    for _ in 0..count {
        let _name = u2(bytes, at);
        let length = u4(bytes, at) as usize;
        *at += length;
    }
}

fn skip_verification(body: &[u8], at: &mut usize) {
    let tag = body[*at];
    *at += 1;
    match tag {
        0..=6 => {}
        7 | 8 => *at += 2,
        other => panic!("reserved verification type {other}"),
    }
}

fn parse_frames(body: &[u8]) -> Vec<u32> {
    let mut at = 0usize;
    let count = u2(body, &mut at) as usize;
    let mut offsets: Vec<u32> = Vec::with_capacity(count);
    for _ in 0..count {
        let frame_type = body[at];
        at += 1;
        let delta = match frame_type {
            0..=63 => frame_type as u32,
            64..=127 => {
                skip_verification(body, &mut at);
                (frame_type - 64) as u32
            }
            247 => {
                let delta = u2(body, &mut at) as u32;
                skip_verification(body, &mut at);
                delta
            }
            248..=251 => u2(body, &mut at) as u32,
            252..=254 => {
                let delta = u2(body, &mut at) as u32;
                for _ in 0..frame_type - 251 {
                    skip_verification(body, &mut at);
                }
                delta
            }
            255 => {
                let delta = u2(body, &mut at) as u32;
                for _ in 0..u2(body, &mut at) {
                    skip_verification(body, &mut at);
                }
                for _ in 0..u2(body, &mut at) {
                    skip_verification(body, &mut at);
                }
                delta
            }
            other => panic!("reserved stack-map frame type {other}"),
        };
        let offset = match offsets.last() {
            Some(previous) => previous + delta + 1,
            None => delta,
        };
        offsets.push(offset);
    }
    offsets
}

fn parse_code_body(pool: &[Option<String>], body: &[u8]) -> CodeView {
    let mut at = 0usize;
    let max_stack = u2(body, &mut at);
    let max_locals = u2(body, &mut at);
    let length = u4(body, &mut at) as usize;
    let code = body[at..at + length].to_vec();
    at += length;
    let handler_count = u2(body, &mut at) as usize;
    let mut handlers = Vec::with_capacity(handler_count);
    for _ in 0..handler_count {
        let start = u2(body, &mut at);
        let end = u2(body, &mut at);
        let handler = u2(body, &mut at);
        let _catch_type = u2(body, &mut at);
        handlers.push((start, end, handler));
    }
    let attribute_count = u2(body, &mut at) as usize;
    let mut frames = Vec::new();
    for _ in 0..attribute_count {
        let name = u2(body, &mut at) as usize;
        let length = u4(body, &mut at) as usize;
        let start = at;
        at += length;
        if pool[name].as_deref() == Some("StackMapTable") {
            frames = parse_frames(&body[start..start + length]);
        }
    }
    CodeView {
        code,
        max_stack,
        max_locals,
        handlers,
        frames,
    }
}

fn code_view(class: &[u8], name: &str, descriptor: &str) -> CodeView {
    assert_eq!(&class[..4], &[0xCA, 0xFE, 0xBA, 0xBE], "bad class magic");
    let mut at = 8usize;
    let pool = read_pool(class, &mut at);
    at += 6; // access_flags, this_class, super_class
    let interfaces = u2(class, &mut at) as usize;
    at += interfaces * 2;
    let fields = u2(class, &mut at) as usize;
    for _ in 0..fields {
        at += 6;
        skip_attributes(class, &mut at);
    }
    let methods = u2(class, &mut at) as usize;
    for _ in 0..methods {
        let _access = u2(class, &mut at);
        let name_index = u2(class, &mut at) as usize;
        let descriptor_index = u2(class, &mut at) as usize;
        let attribute_count = u2(class, &mut at) as usize;
        for _ in 0..attribute_count {
            let attribute_name = u2(class, &mut at) as usize;
            let length = u4(class, &mut at) as usize;
            let start = at;
            at += length;
            if pool[name_index].as_deref() == Some(name)
                && pool[descriptor_index].as_deref() == Some(descriptor)
                && pool[attribute_name].as_deref() == Some("Code")
            {
                return parse_code_body(&pool, &class[start..start + length]);
            }
        }
    }
    panic!("class has no Code for {name}{descriptor}");
}

/// The byte size of one instruction, or `None` for a switch.
fn instruction_size(opcode: u8) -> Option<usize> {
    Some(match opcode {
        0x00..=0x0f => 1,
        0x10 => 2,
        0x11 => 3,
        0x12 => 2,
        0x13 | 0x14 => 3,
        0x15..=0x19 => 2,
        0x1a..=0x35 => 1,
        0x36..=0x3a => 2,
        0x3b..=0x83 => 1,
        0x84 => 3,
        0x85..=0x98 => 1,
        0x99..=0xa8 => 3,
        0xa9 => 2,
        0xaa | 0xab => return None,
        0xac..=0xb1 => 1,
        0xb2..=0xb8 => 3,
        0xb9 | 0xba => 5,
        0xbb => 3,
        0xbc => 2,
        0xbd => 3,
        0xbe | 0xbf => 1,
        0xc0 | 0xc1 => 3,
        0xc2 | 0xc3 => 1,
        0xc5 => 4,
        0xc6..=0xc9 => 3,
        _ => return None,
    })
}

/// Every instruction start, and `(offset, target)` for every branch.
fn walk(code: &[u8]) -> (BTreeSet<usize>, Vec<(usize, usize)>) {
    let mut starts = BTreeSet::new();
    let mut branches = Vec::new();
    let mut at = 0usize;
    while at < code.len() {
        starts.insert(at);
        let opcode = code[at];
        let size = instruction_size(opcode)
            .unwrap_or_else(|| panic!("unexpected opcode 0x{opcode:02x} at {at}"));
        let target = match opcode {
            0x99..=0xa8 | 0xc6 | 0xc7 => {
                let relative = i16::from_be_bytes([code[at + 1], code[at + 2]]) as i64;
                Some(at as i64 + relative)
            }
            0xc8 | 0xc9 => {
                let relative =
                    i32::from_be_bytes([code[at + 1], code[at + 2], code[at + 3], code[at + 4]])
                        as i64;
                Some(at as i64 + relative)
            }
            _ => None,
        };
        if let Some(target) = target {
            assert!(target >= 0, "branch at {at} targets {target}");
            branches.push((at, target as usize));
        }
        at += size;
    }
    (starts, branches)
}

/// Every branch, exception range and frame must still name an instruction.
fn assert_relocated(source: &CodeView, woven: &CodeView, label: &str) {
    let (starts, branches) = walk(&woven.code);
    assert_eq!(
        branches.len(),
        walk(&source.code).1.len(),
        "{label}: branch count"
    );
    for (at, target) in &branches {
        assert!(
            starts.contains(target),
            "{label}: branch at {at} targets {target}, which is not an instruction start"
        );
    }
    assert_eq!(
        woven.handlers.len(),
        source.handlers.len(),
        "{label}: handler count"
    );
    for (start, end, handler) in &woven.handlers {
        assert!(*start < *end, "{label}: exception range {start}..{end}");
        assert!(
            *end as usize <= woven.code.len(),
            "{label}: exception range ends past the code"
        );
        assert!(
            starts.contains(&(*start as usize)),
            "{label}: exception range starts at {start}, which is not an instruction start"
        );
        assert!(
            *end as usize == woven.code.len() || starts.contains(&(*end as usize)),
            "{label}: exception range ends at {end}, which is not an instruction start"
        );
        assert!(
            starts.contains(&(*handler as usize)),
            "{label}: handler at {handler}, which is not an instruction start"
        );
    }
    assert_eq!(
        woven.frames.len(),
        source.frames.len(),
        "{label}: frame count"
    );
    let mut previous = None;
    for frame in &woven.frames {
        assert!(
            starts.contains(&(*frame as usize)),
            "{label}: frame at {frame}, which is not an instruction start"
        );
        assert!(
            previous.is_none_or(|previous| *frame > previous),
            "{label}: frames are not in increasing order"
        );
        previous = Some(*frame);
    }
}

/// A `HEAD` insert lands at offset 0, so every recorded offset moves by the
/// insert length and the branch deltas stay the same.
fn assert_head_shift(source: &CodeView, woven: &CodeView, label: &str) {
    let delta = (woven.code.len() - source.code.len()) as u32;
    assert!(delta > 0, "{label}: nothing was inserted");
    let (_, source_branches) = walk(&source.code);
    let (_, woven_branches) = walk(&woven.code);
    for ((_, before), (_, after)) in source_branches.iter().zip(&woven_branches) {
        assert_eq!(
            *after as u32,
            *before as u32 + delta,
            "{label}: branch target"
        );
    }
    for ((start, end, handler), (woven_start, woven_end, woven_handler)) in
        source.handlers.iter().zip(&woven.handlers)
    {
        assert_eq!(
            *woven_start as u32,
            *start as u32 + delta,
            "{label}: range start"
        );
        assert_eq!(*woven_end as u32, *end as u32 + delta, "{label}: range end");
        assert_eq!(
            *woven_handler as u32,
            *handler as u32 + delta,
            "{label}: handler"
        );
    }
    for (before, after) in source.frames.iter().zip(&woven.frames) {
        assert_eq!(*after, *before + delta, "{label}: frame");
    }
}

fn weave_head_and_check(
    classes: &Path,
    name: &str,
    descriptor: &str,
    callback_name: &str,
) -> Vec<u8> {
    let bytes = read_class(classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_head(
        &class,
        &method("demo/Target", name, descriptor),
        &callback(callback_name, "(I)V"),
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "{name} round-trips");
    let before = code_view(&bytes, name, descriptor);
    let after = code_view(&woven, name, descriptor);
    assert_relocated(&before, &after, name);
    assert_head_shift(&before, &after, name);
    assert!(
        after.max_stack > before.max_stack,
        "{name}: max_stack was not raised"
    );
    assert!(after.max_locals >= before.max_locals);
    woven
}

#[test]
fn head_relocates_branches_loops_and_handlers() {
    let classes = compile("head");
    for (name, descriptor) in [
        ("classify", "(I)I"),
        ("loop", "(I)I"),
        ("guarded", "(I)I"),
        ("multi", "(I)I"),
    ] {
        let before = code_view(&read_class(&classes, "demo/Target"), name, descriptor);
        assert!(
            !before.frames.is_empty(),
            "{name}: fixture is expected to carry a StackMapTable"
        );
        weave_head_and_check(
            &classes,
            name,
            descriptor,
            match name {
                "classify" => "classify",
                "loop" => "loop",
                "guarded" => "guarded",
                _ => "multi",
            },
        );
    }
    // The `guarded` fixture really carries a handler; without one the range
    // assertions above would pass vacuously.
    let guarded = code_view(&read_class(&classes, "demo/Target"), "guarded", "(I)I");
    assert_eq!(guarded.handlers.len(), 1);
}

#[test]
fn return_relocates_every_return_in_a_branchy_void_method() {
    let classes = compile("return");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "accumulate", "(I)V"),
        &callback("accumulate", "(I)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let before = code_view(&bytes, "accumulate", "(I)V");
    let after = code_view(&woven, "accumulate", "(I)V");
    assert_relocated(&before, &after, "accumulate");
    assert_eq!(after.handlers.len(), 0, "this fixture has no handler");
    assert_eq!(after.frames.len(), before.frames.len());
    // Two returns, so two inserts of `iload_0; invokestatic` (5 bytes each).
    assert_eq!(after.code.len(), before.code.len() + 10);
    assert_eq!(
        after.code.iter().filter(|opcode| **opcode == 0xb8).count(),
        2
    );
}

#[test]
fn tail_relocates_only_the_last_return() {
    let classes = compile("tail");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "accumulate", "(I)V"),
        &callback("accumulate", "(I)V"),
        ReturnMode::Tail,
    )
    .unwrap();
    let before = code_view(&bytes, "accumulate", "(I)V");
    let after = code_view(&woven, "accumulate", "(I)V");
    assert_relocated(&before, &after, "accumulate tail");
    assert_eq!(after.code.len(), before.code.len() + 5);
    assert_eq!(
        after.code.iter().filter(|opcode| **opcode == 0xb8).count(),
        1
    );
}

#[test]
fn switches_and_wide_locals_are_now_weaveable() {
    let classes = compile("unsupported");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    // `switchy` and the 300-local `Wide.wide` used to be rejected here.  The
    // weaver now re-derives a switch's padding and walks `wide` instructions, so
    // both weave; `fixture_weaver_switches.rs` checks their results in a JVM.
    let woven = weave_head(
        &class,
        &method("demo/Target", "switchy", "(I)I"),
        &callback("classify", "(I)V"),
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let code = reparsed
        .method("switchy", "(I)I")
        .unwrap()
        .code
        .clone()
        .unwrap();
    assert!(
        code.iter()
            .take(16)
            .any(|opcode| matches!(opcode, 0xaa | 0xab)),
        "the woven switchy has no switch: {code:02x?}"
    );

    let wide_bytes = read_class(&classes, "demo/Wide");
    let wide = ClassFile::parse(&wide_bytes).unwrap();
    let woven = weave_head(
        &wide,
        &method("demo/Wide", "wide", "()I"),
        &MethodRef {
            owner: "demo/Callback".into(),
            name: "calls".into(),
            descriptor: "()V".into(),
        },
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the wide fixture round-trips"
    );
}

#[test]
fn condy_is_rejected_with_a_reason() {
    let classes = compile("condy");
    let bytes = add_condy(read_class(&classes, "demo/Target"));
    let class = ClassFile::parse(&bytes).unwrap();
    let error = weave_head(
        &class,
        &method("demo/Target", "classify", "(I)I"),
        &callback("classify", "(I)V"),
    )
    .unwrap_err();
    assert!(error.contains("CONSTANT_Dynamic"), "{error}");
}

/// Append one unused `CONSTANT_Dynamic` entry to a class's pool.
///
/// `javac` never emits condy on its own.  The entry's operands point at nothing
/// the weaver reads: the rejection is decided by the tag, and the class still
/// parses because the pool is a prefix of the file.
fn add_condy(bytes: Vec<u8>) -> Vec<u8> {
    let mut at = 10usize;
    let mut pool = bytes.clone();
    let count = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    let mut index = 1;
    while index < count {
        let tag = pool[at];
        at += 1;
        at += match tag {
            1 => 2 + u16::from_be_bytes([pool[at], pool[at + 1]]) as usize,
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 4,
            5 | 6 => {
                index += 1;
                8
            }
            7 | 8 | 16 | 19 | 20 => 2,
            15 => 3,
            other => panic!("unexpected constant pool tag {other}"),
        };
        index += 1;
    }
    pool.splice(at..at, [17u8, 0, 0, 0, 0]);
    pool[8..10].copy_from_slice(&((count + 1) as u16).to_be_bytes());
    pool
}

const RUNNER: &str = r#"import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.nio.file.Paths;

class Runner {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(args[0]);
        String name = args[1];
        int argument = Integer.parseInt(args[2]);
        try (URLClassLoader loader =
                new URLClassLoader(new URL[] { dir.toUri().toURL() }, Runner.class.getClassLoader())) {
            Class<?> target = Class.forName("demo.Target", true, loader);
            Method woven = target.getMethod(name, int.class);
            Object result = woven.invoke(null, argument);
            Class<?> callback = Class.forName("demo.Callback", true, loader);
            int calls = (Integer) callback.getMethod("calls").invoke(null);
            System.out.println(
                    "result=" + result
                    + " calls=" + calls
                    + " total=" + target.getField("total").getInt(null));
        }
    }
}
"#;

/// Load a woven class in a fresh loader and invoke it, returning the runner's
/// `result=... calls=... total=...` line.
fn run(classes: &Path, woven: &[u8], name: &str, argument: i32, label: &str) -> Option<String> {
    let java = java()?;
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-frames-run-{}-{label}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("demo")).unwrap();
    for class in ["demo/Target", "demo/Callback"] {
        fs::copy(
            classes.join(format!("{class}.class")),
            root.join(format!("{class}.class")),
        )
        .unwrap();
    }
    fs::write(root.join("demo/Target.class"), woven).unwrap();
    fs::write(root.join("Runner.java"), RUNNER).unwrap();
    let output = Command::new(java)
        .arg(root.join("Runner.java"))
        .arg(&root)
        .arg(name)
        .arg(argument.to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{label}: the woven class did not load or run:\n{}\n{stdout}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
        .lines()
        .find(|line| line.starts_with("result="))
        .map(str::to_owned)
}

#[test]
fn woven_branchy_methods_verify_and_keep_their_result() {
    let classes = compile("runtime");
    if java().is_none() {
        return;
    }
    for (name, callback_name, argument, expected) in [
        ("classify", "classify", 5, "result=1 calls=1 total=0"),
        ("loop", "loop", 4, "result=6 calls=1 total=0"),
        ("guarded", "guarded", 0, "result=-1 calls=1 total=0"),
        ("guarded", "guarded", 5, "result=20 calls=1 total=0"),
        ("multi", "multi", -3, "result=3 calls=1 total=0"),
    ] {
        let woven = weave_head_and_check(&classes, name, "(I)I", callback_name);
        let line = run(&classes, &woven, name, argument, name).unwrap();
        assert_eq!(line, expected, "{name}({argument})");
    }
}

#[test]
fn weave_class_stacks_head_and_return_on_a_framed_method() {
    let classes = compile("stacked");
    let Some(_) = java() else {
        return;
    };
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = method("demo/Target", "accumulate", "(I)V");
    let call = callback("accumulate", "(I)V");
    // HEAD first, then RETURN: the second pass relocates the frames and
    // branches the first pass already moved.
    let plans = [
        MixinInjectionPlan::inject_at_return(target.clone(), call.clone()),
        MixinInjectionPlan::inject_at_head(target, call),
    ];
    let woven = weave_class(&class, &plans).unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let before = code_view(&bytes, "accumulate", "(I)V");
    let after = code_view(&woven, "accumulate", "(I)V");
    assert_relocated(&before, &after, "accumulate stacked");
    let line = run(&classes, &woven, "accumulate", 3, "stacked").unwrap();
    // One HEAD callback and the one return that actually runs.
    assert_eq!(line, "result=null calls=2 total=1006");
}

#[test]
fn woven_return_callback_verifies_and_runs() {
    let classes = compile("runtime-return");
    let Some(_) = java() else {
        return;
    };
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "accumulate", "(I)V"),
        &callback("accumulate", "(I)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let line = run(&classes, &woven, "accumulate", 3, "accumulate").unwrap();
    // The loop and the `n > 10` branch survive; only the returning path runs
    // one RETURN callback.
    assert_eq!(line, "result=null calls=1 total=1006");
}

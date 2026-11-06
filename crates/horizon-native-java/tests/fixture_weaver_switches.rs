//! Switch and `wide` relocation tests for the bounded weaver.
//!
//! `tableswitch`/`lookupswitch` carry their own alignment padding, so an insert
//! that moves one changes its size as well as every offset after it.  The
//! fixtures come from `javac`: a dense switch (a `tableswitch` with a jump
//! table), a sparse one (a `lookupswitch`), a switch inside a loop, a `void`
//! switch with several returns, a switch whose case owns an exception handler,
//! and a method with more than 255 local slots, which is what forces `wide`.
//!
//! Each woven class is parsed back and every offset a rewrite touches - branch
//! targets, switch targets, exception ranges, stack-map frames - is checked to
//! land on an instruction boundary, with the switch's own padding re-derived
//! from its new offset.  When a JDK is present the rewritten class is also
//! loaded in a fresh `URLClassLoader`, invoked, and its callback counter read
//! back: the verifier is the real judge of a relocated switch.

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
    public static int dense(int x) {
        switch (x) {
            case 0: return 10;
            case 1: return 11;
            case 2: return 12;
            case 3: return 13;
            default: return -1;
        }
    }

    public static int sparse(int x) {
        switch (x) {
            case 1: return 100;
            case 100: return 200;
            case 10000: return 300;
            default: return 0;
        }
    }

    public static int loops(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            switch (i % 3) {
                case 0: sum += 1; break;
                case 1: sum -= 1; break;
                default: sum += 10;
            }
        }
        return sum;
    }

    public static int guarded(int x) {
        int result;
        switch (x) {
            case 0:
                try { result = 100 / x; } catch (ArithmeticException e) { result = -2; }
                break;
            case 1:
                result = 7;
                break;
            case 2:
                result = 8;
                break;
            default:
                result = -1;
        }
        if (result > 5) {
            return result * 2;
        }
        return result;
    }

    public static void counted(int x) {
        switch (x) {
            case 0: return;
            case 1: return;
            default: break;
        }
        if (x > 100) {
            return;
        }
    }
}"#;

const CALLBACK: &str = r#"package demo;
public final class Callback {
    private static int calls;

    public static int calls() { return calls; }
    public static void dense(int x) { calls++; }
    public static void sparse(int x) { calls++; }
    public static void loops(int n) { calls++; }
    public static void guarded(int x) { calls++; }
    public static void guardedVoid(int x) { calls++; }
    public static void counted(int x) { calls++; }
    public static void wide() { calls++; }
}"#;

/// A `void` method with a jump table, an exception handler and three returns:
/// the shape a `RETURN` plan has to relocate through a handler.
const GUARD: &str = r#"package demo;
public class Guard {
    public static int sink;

    public static void guardedVoid(int x) {
        switch (x) {
            case 0:
                try { sink = 100 / x; } catch (ArithmeticException e) { sink = -2; }
                return;
            case 1:
                sink = 1;
                return;
            default:
                break;
        }
        sink = 2;
    }
}"#;

/// A method with 300 `int` locals, so `javac` emits `wide` load/store for every
/// slot at or above 256.  The sum is 0 + 1 + ... + 299 = 44850.
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
        "horizon-native-java-weaver-switches-{name}-{}",
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
        ("Guard.java", GUARD.to_owned()),
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
        .arg(src.join("Guard.java"))
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

/// The alignment padding a switch at `at` needs, exactly as the class file
/// format defines it: the first operand starts on a 4-byte boundary counted
/// from the start of the `code` array.
fn switch_padding(at: usize) -> usize {
    (4 - (at + 1) % 4) % 4
}

/// One walk of a method's code: its instruction starts, its switch sites and
/// every branch/switch target.
struct Walk {
    starts: BTreeSet<usize>,
    /// `(site, target)` for every branch and every switch target.
    targets: Vec<(usize, usize)>,
    /// `(offset, padding, absolute targets)` per switch.
    switches: Vec<(usize, usize, Vec<usize>)>,
}

fn resolve(code: &[u8], at: usize, delta: i64) -> usize {
    let target = at as i64 + delta;
    assert!(
        target >= 0 && (target as usize) < code.len(),
        "the instruction at {at} targets {target}, outside the {}-byte code",
        code.len()
    );
    target as usize
}

fn read_i32(code: &[u8], at: usize) -> i32 {
    i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

fn fixed_size(opcode: u8) -> usize {
    match opcode {
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
        other => panic!("unexpected opcode 0x{other:02x}"),
    }
}

fn walk(code: &[u8]) -> Walk {
    let mut walk = Walk {
        starts: BTreeSet::new(),
        targets: Vec::new(),
        switches: Vec::new(),
    };
    let mut at = 0usize;
    while at < code.len() {
        walk.starts.insert(at);
        let opcode = code[at];
        match opcode {
            0xaa => {
                let header = at + 1 + switch_padding(at);
                assert_eq!(
                    header % 4,
                    0,
                    "tableswitch at {at} puts its operands on an unaligned boundary"
                );
                let default = read_i32(code, header);
                let low = read_i32(code, header + 4);
                let high = read_i32(code, header + 8);
                let mut field = header + 12;
                let mut targets = vec![resolve(code, at, i64::from(default))];
                for _ in low..=high {
                    targets.push(resolve(code, at, i64::from(read_i32(code, field))));
                    field += 4;
                }
                walk.targets
                    .extend(targets.iter().map(|target| (at, *target)));
                walk.switches.push((at, switch_padding(at), targets));
                at = field;
            }
            0xab => {
                let header = at + 1 + switch_padding(at);
                assert_eq!(
                    header % 4,
                    0,
                    "lookupswitch at {at} puts its operands on an unaligned boundary"
                );
                let default = read_i32(code, header);
                let pairs = read_i32(code, header + 4);
                assert!(pairs >= 0, "lookupswitch at {at} has {pairs} pairs");
                let mut field = header + 8;
                let mut targets = vec![resolve(code, at, i64::from(default))];
                for _ in 0..pairs {
                    field += 4; // match key
                    targets.push(resolve(code, at, i64::from(read_i32(code, field))));
                    field += 4;
                }
                walk.targets
                    .extend(targets.iter().map(|target| (at, *target)));
                walk.switches.push((at, switch_padding(at), targets));
                at = field;
            }
            0xc4 => {
                let sub = code[at + 1];
                at += match sub {
                    0x15..=0x19 | 0x36..=0x3a | 0xa9 => 4,
                    0x84 => 6,
                    other => panic!("wide prefixes opcode 0x{other:02x} at {at}"),
                };
            }
            0x99..=0xa8 | 0xc6 | 0xc7 => {
                let delta = i16::from_be_bytes([code[at + 1], code[at + 2]]);
                walk.targets.push((at, resolve(code, at, i64::from(delta))));
                at += 3;
            }
            0xc8 | 0xc9 => {
                let delta = read_i32(code, at + 1);
                walk.targets.push((at, resolve(code, at, i64::from(delta))));
                at += 5;
            }
            _ => at += fixed_size(opcode),
        }
    }
    walk
}

/// Every branch target, switch target, exception range and frame must still name
/// an instruction boundary, and every switch operand must stay 4-byte aligned.
fn assert_relocated(source: &CodeView, woven: &CodeView, label: &str) {
    let before = walk(&source.code);
    let after = walk(&woven.code);
    assert_eq!(
        after.targets.len(),
        before.targets.len(),
        "{label}: branch and switch target count"
    );
    assert_eq!(
        after.switches.len(),
        before.switches.len(),
        "{label}: switch count"
    );
    for (site, target) in &after.targets {
        assert!(
            after.starts.contains(target),
            "{label}: the instruction at {site} targets {target}, which is not an instruction start"
        );
    }
    for (index, (_, after_pad, _)) in after.switches.iter().enumerate() {
        // The padding has to be what the new offset asks for, which the walker
        // asserts while it decodes; pin that it is not simply carried over when
        // the offset moved.
        assert_eq!(
            *after_pad,
            switch_padding(after.switches[index].0),
            "{label}: switch {index} padding"
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
            after.starts.contains(&(*start as usize)),
            "{label}: exception range starts at {start}, which is not an instruction start"
        );
        assert!(
            *end as usize == woven.code.len() || after.starts.contains(&(*end as usize)),
            "{label}: exception range ends at {end}, which is not an instruction start"
        );
        assert!(
            after.starts.contains(&(*handler as usize)),
            "{label}: handler at {handler}, which is not an instruction start"
        );
    }

    assert!(
        woven.frames.len() >= source.frames.len(),
        "{label}: a frame was dropped"
    );
    let mut previous = None;
    for frame in &woven.frames {
        assert!(
            after.starts.contains(&(*frame as usize)),
            "{label}: frame at {frame}, which is not an instruction start"
        );
        assert!(
            previous.is_none_or(|previous| *frame > previous),
            "{label}: frames are not in increasing order"
        );
        previous = Some(*frame);
    }

    // Inserting callback bytes grows the code; re-padding a switch adds or
    // removes up to three bytes per switch on top of that.  Anything else means
    // an instruction changed size without the layout saying so.
    let inserts = (woven.code.len() as i64 - source.code.len() as i64)
        - (after
            .switches
            .iter()
            .map(|(_, pad, _)| *pad as i64)
            .sum::<i64>()
            - before
                .switches
                .iter()
                .map(|(_, pad, _)| *pad as i64)
                .sum::<i64>());
    assert!(
        inserts > 0,
        "{label}: nothing was inserted (the code changed by {inserts} bytes)"
    );
}

const RUNNER: &str = r#"import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.nio.file.Paths;

class Runner {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(args[0]);
        String owner = args[1];
        String name = args[2];
        try (URLClassLoader loader =
                new URLClassLoader(new URL[] { dir.toUri().toURL() }, Runner.class.getClassLoader())) {
            Class<?> target = Class.forName(owner, true, loader);
            Method woven;
            Object result;
            if (args.length > 3) {
                woven = target.getMethod(name, int.class);
                result = woven.invoke(null, Integer.parseInt(args[3]));
            } else {
                woven = target.getMethod(name);
                result = woven.invoke(null);
            }
            Class<?> callback = Class.forName("demo.Callback", true, loader);
            int calls = (Integer) callback.getMethod("calls").invoke(null);
            String sink = "";
            try {
                sink = " sink=" + target.getField("sink").getInt(null);
            } catch (NoSuchFieldException absent) {
                // The owner has no observable field; the result says it all.
            }
            System.out.println("result=" + result + " calls=" + calls + sink);
        }
    }
}
"#;

/// Load a woven class in a fresh loader and invoke it, returning the runner's
/// `result=... calls=...` line.
fn run(
    classes: &Path,
    woven: &[u8],
    owner: &str,
    name: &str,
    argument: Option<i32>,
    label: &str,
) -> Option<String> {
    let java = java()?;
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-switches-run-{}-{label}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("demo")).unwrap();
    for entry in fs::read_dir(classes.join("demo")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "class")
        {
            fs::copy(&path, root.join("demo").join(path.file_name().unwrap())).unwrap();
        }
    }
    fs::write(root.join(format!("{owner}.class")), woven).unwrap();
    fs::write(root.join("Runner.java"), RUNNER).unwrap();
    let mut command = Command::new(java);
    command
        .arg(root.join("Runner.java"))
        .arg(&root)
        .arg(owner.replace('/', "."))
        .arg(name);
    if let Some(argument) = argument {
        command.arg(argument.to_string());
    }
    let output = command.output().unwrap();
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

/// Weave a `HEAD` callback and check the structure of the result.
fn head_and_check(
    classes: &Path,
    owner: &str,
    name: &str,
    descriptor: &str,
    callback_name: &str,
    callback_descriptor: &str,
) -> Vec<u8> {
    let bytes = read_class(classes, owner);
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_head(
        &class,
        &method(owner, name, descriptor),
        &callback(callback_name, callback_descriptor),
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "{name} round-trips");
    let before = code_view(&bytes, name, descriptor);
    let after = code_view(&woven, name, descriptor);
    assert_relocated(&before, &after, name);
    assert!(
        after.max_stack > before.max_stack,
        "{name}: max_stack was not raised"
    );
    woven
}

#[test]
fn head_relocates_a_dense_tableswitch() {
    let classes = compile("dense");
    let before = code_view(&read_class(&classes, "demo/Target"), "dense", "(I)I");
    assert!(
        walk(&before.code).switches.len() == 1,
        "the dense fixture is expected to compile to one jump table"
    );
    assert_eq!(before.code[walk(&before.code).switches[0].0], 0xaa);

    let woven = head_and_check(&classes, "demo/Target", "dense", "(I)I", "dense", "(I)V");
    let after = code_view(&woven, "dense", "(I)I");
    let switches = walk(&after.code).switches;
    assert_eq!(switches.len(), 1);
    // Four cases plus the default.
    assert_eq!(switches[0].2.len(), 5);

    if java().is_none() {
        return;
    }
    for (argument, expected) in [(0, 10), (2, 12), (3, 13), (9, -1)] {
        let line = run(
            &classes,
            &woven,
            "demo/Target",
            "dense",
            Some(argument),
            "dense",
        )
        .unwrap();
        assert_eq!(
            line,
            format!("result={expected} calls=1"),
            "dense({argument})"
        );
    }
}

#[test]
fn head_relocates_a_sparse_lookupswitch() {
    let classes = compile("sparse");
    let before = code_view(&read_class(&classes, "demo/Target"), "sparse", "(I)I");
    let before_walk = walk(&before.code);
    assert_eq!(before_walk.switches.len(), 1);
    assert_eq!(before_walk.switches[0].2.len(), 4); // default plus three keys
    assert_eq!(before.code[before_walk.switches[0].0], 0xab);

    let woven = head_and_check(&classes, "demo/Target", "sparse", "(I)I", "sparse", "(I)V");
    let after = code_view(&woven, "sparse", "(I)I");
    assert_eq!(walk(&after.code).switches[0].2.len(), 4);

    if java().is_none() {
        return;
    }
    for (argument, expected) in [(1, 100), (100, 200), (10_000, 300), (7, 0)] {
        let line = run(
            &classes,
            &woven,
            "demo/Target",
            "sparse",
            Some(argument),
            "sparse",
        )
        .unwrap();
        assert_eq!(
            line,
            format!("result={expected} calls=1"),
            "sparse({argument})"
        );
    }
}

#[test]
fn head_relocates_a_switch_inside_a_loop_with_branches() {
    let classes = compile("loops");
    let before = code_view(&read_class(&classes, "demo/Target"), "loops", "(I)I");
    assert!(!before.frames.is_empty(), "the loop needs frames");
    assert_eq!(walk(&before.code).switches.len(), 1);

    let woven = head_and_check(&classes, "demo/Target", "loops", "(I)I", "loops", "(I)V");
    if java().is_none() {
        return;
    }
    // i = 0..3 mapped through i % 3 gives +1, -1, +10, +1.
    for (argument, expected) in [(4, 11), (0, 0), (7, 21)] {
        let line = run(
            &classes,
            &woven,
            "demo/Target",
            "loops",
            Some(argument),
            "loops",
        )
        .unwrap();
        assert_eq!(
            line,
            format!("result={expected} calls=1"),
            "loops({argument})"
        );
    }
}

#[test]
fn return_relocates_a_switch_with_an_exception_handler() {
    let classes = compile("guarded-void");
    let bytes = read_class(&classes, "demo/Guard");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Guard", "guardedVoid", "(I)V"),
        &callback("guardedVoid", "(I)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let before = code_view(&bytes, "guardedVoid", "(I)V");
    let after = code_view(&woven, "guardedVoid", "(I)V");
    assert_eq!(before.handlers.len(), 1, "this fixture owns a handler");
    assert_eq!(walk(&before.code).switches.len(), 1);
    assert_relocated(&before, &after, "guardedVoid");
    // Three returns: the two case bodies and the fall-through.
    assert_eq!(
        after.code.iter().filter(|opcode| **opcode == 0xb8).count(),
        3
    );

    if java().is_none() {
        return;
    }
    for (argument, sink) in [(0, -2), (1, 1), (5, 2)] {
        let line = run(
            &classes,
            &woven,
            "demo/Guard",
            "guardedVoid",
            Some(argument),
            "guardedVoid",
        )
        .unwrap();
        assert_eq!(
            line,
            format!("result=null calls=1 sink={sink}"),
            "guardedVoid({argument})"
        );
    }
}

#[test]
fn return_rejects_a_non_void_target_with_a_precise_reason() {
    let classes = compile("non-void");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    assert_eq!(class.name, "demo/Target");
    // `guarded` also has a switch and a handler, and the rejection happens
    // before either is touched: the callback cannot be built at all.
    let error = weave_return(
        &class,
        &method("demo/Target", "guarded", "(I)I"),
        &callback("guarded", "(I)V"),
        ReturnMode::Return,
    )
    .unwrap_err();
    assert!(error.contains("non-void"), "{error}");
    assert!(error.contains("CallbackInfoReturnable"), "{error}");
    assert!(error.contains("StackMapTable"), "{error}");
}

#[test]
fn head_relocates_a_switch_with_an_exception_handler() {
    let classes = compile("guarded-head");
    let before = code_view(&read_class(&classes, "demo/Target"), "guarded", "(I)I");
    assert_eq!(before.handlers.len(), 1, "this fixture owns a handler");
    assert_eq!(walk(&before.code).switches.len(), 1);
    assert!(!before.frames.is_empty(), "the handler needs frames");

    let woven = head_and_check(
        &classes,
        "demo/Target",
        "guarded",
        "(I)I",
        "guarded",
        "(I)V",
    );
    let after = code_view(&woven, "guarded", "(I)I");
    assert_eq!(after.handlers.len(), 1);
    assert_eq!(after.frames.len(), before.frames.len());

    if java().is_none() {
        return;
    }
    for (argument, expected) in [(0, -2), (1, 14), (2, 16), (7, -1)] {
        let line = run(
            &classes,
            &woven,
            "demo/Target",
            "guarded",
            Some(argument),
            "guarded",
        )
        .unwrap();
        assert_eq!(
            line,
            format!("result={expected} calls=1"),
            "guarded({argument})"
        );
    }
}

#[test]
fn return_weaves_inside_a_switch_and_runs() {
    let classes = compile("counted");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "counted", "(I)V"),
        &callback("counted", "(I)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let before = code_view(&bytes, "counted", "(I)V");
    let after = code_view(&woven, "counted", "(I)V");
    assert_relocated(&before, &after, "counted");
    assert_eq!(walk(&before.code).switches.len(), 1);
    // Three explicit returns plus the switch's own fall-through return.
    let returns = after.code.iter().filter(|opcode| **opcode == 0xb1).count();
    assert_eq!(returns, 4, "{:02x?}", after.code);

    if java().is_none() {
        return;
    }
    for argument in [0, 1, 5, 200] {
        let line = run(
            &classes,
            &woven,
            "demo/Target",
            "counted",
            Some(argument),
            "counted",
        )
        .unwrap();
        assert_eq!(line, "result=null calls=1", "counted({argument})");
    }
}

#[test]
fn head_weaves_a_method_with_wide_locals() {
    let classes = compile("wide");
    let bytes = read_class(&classes, "demo/Wide");
    let before = code_view(&bytes, "wide", "()I");
    assert!(
        before.max_locals > 255,
        "the fixture is expected to need wide local slots"
    );
    assert!(
        before.code.windows(2).any(|pair| pair[0] == 0xc4),
        "the fixture is expected to carry a wide instruction"
    );

    let woven = head_and_check(&classes, "demo/Wide", "wide", "()I", "wide", "()V");
    let after = code_view(&woven, "wide", "()I");
    assert_eq!(
        after.max_locals, before.max_locals,
        "wide slots already cover the arguments"
    );
    // `wide iload <slot>` and `wide istore <slot>` survive verbatim: the insert
    // only moves them, it never re-encodes a local operand.
    assert_eq!(
        after.code.iter().filter(|opcode| **opcode == 0xc4).count(),
        before.code.iter().filter(|opcode| **opcode == 0xc4).count()
    );

    if java().is_none() {
        return;
    }
    let line = run(&classes, &woven, "demo/Wide", "wide", None, "wide").unwrap();
    assert_eq!(line, "result=44850 calls=1");
}

#[test]
fn weave_class_stacks_head_and_return_on_a_switch() {
    let classes = compile("stacked");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = method("demo/Target", "counted", "(I)V");
    let call = callback("counted", "(I)V");
    // HEAD first, then RETURN: the second pass relocates the switch, the frames
    // and the branches the first pass already moved.
    let plans = [
        MixinInjectionPlan::inject_at_return(target.clone(), call.clone()),
        MixinInjectionPlan::inject_at_head(target, call),
    ];
    let woven = weave_class(&class, &plans).unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let before = code_view(&bytes, "counted", "(I)V");
    let after = code_view(&woven, "counted", "(I)V");
    assert_relocated(&before, &after, "counted stacked");
    assert_eq!(walk(&after.code).switches.len(), 1);

    if java().is_none() {
        return;
    }
    let line = run(
        &classes,
        &woven,
        "demo/Target",
        "counted",
        Some(0),
        "stacked",
    )
    .unwrap();
    // One HEAD callback plus the return the switch takes.
    assert_eq!(line, "result=null calls=2");
}

#[test]
fn javac_reports_the_relocated_switches() {
    let classes = compile("javap");
    let Some(javap) = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javap"))
        .or_else(|| Some(PathBuf::from("javap")))
        .filter(|path| Command::new(path).arg("-version").output().is_ok())
    else {
        return;
    };
    let woven = head_and_check(&classes, "demo/Target", "dense", "(I)I", "dense", "(I)V");
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-switches-javap-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("demo")).unwrap();
    fs::write(root.join("demo/Target.class"), woven).unwrap();
    let output = Command::new(javap)
        .arg("-c")
        .arg("-p")
        .arg(root.join("demo/Target.class"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "javap rejected the woven class: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("tableswitch"), "{text}");
    assert!(text.contains("dense"), "{text}");
}

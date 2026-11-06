//! Mixin selector modifiers for the bounded weaver: `ordinal`, `slice`,
//! `shift`/`by` on an `INVOKE` anchor, and the ordinal on `RETURN`/`TAIL`/
//! `HEAD`.
//!
//! One javac target carries the three call sites the ordinals count: two
//! `invokestatic` overloads and one `invokevirtual` overload of the same name.
//! Because they differ only by descriptor, a selector with an empty descriptor
//! matches all three, which is what lets one ordinal pick each of them.  Every
//! woven class is loaded in a fresh `URLClassLoader` and run, so the assertions
//! cover the verifier as well as which site actually ran.  The hand-built
//! classes pin the insertion offsets, the frame table and the exception-range
//! rejection that javac output cannot conveniently express.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, NativeJar, ReturnMode,
    callback_info_returnable_class, weave_class,
    weaver::{
        InvokeSelector, InvokeShift, ReturnSelector, Shift, SiteSelector, SliceBounds, weave_head,
        weave_head_selected, weave_head_shifted, weave_invoke, weave_invoke_at,
        weave_invoke_selected, weave_invoke_shifted, weave_invoke_shifted_by,
        weave_returns_selected, weave_returns_shifted,
    },
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

const TARGET: &str = r#"package demo;

public class Target {
    public static String trace = "";

    public static void hit(int x) { trace += "i"; }
    public static void hit(String x) { trace += "s"; }
    public void hit(long x) { trace += "v"; }

    public void threeSites() {
        hit(1);
        hit("x");
        this.hit(1L);
    }

    public static int pick(int x) {
        if (x > 5) { return 1; }
        if (x > 0) { return 2; }
        return 3;
    }

    public static int twoReturns(int x) {
        if (x > 0) { return 1; }
        return 2;
    }

    public static void twoCalls() {
        hit(1);
        hit(String.valueOf(1));
    }

    public static void twoCallsSlice() {
        hit(1);
        hit(String.valueOf(1));
    }

    public static void twoCallsShift() {
        hit(1);
        hit(String.valueOf(1));
    }
}"#;

const CALLBACK: &str = r#"package demo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

public final class Callback {
    public static int calls;
    public static String seen = "";
    public static Object value;

    public static void onCall() { calls++; seen = Target.trace; }
    public static void onPlain(int x) { calls++; seen = "plain"; }
    public static void onReturn(int x, CallbackInfoReturnable cir) {
        calls++;
        value = cir.getReturnValue();
    }
}"#;

const CALLBACK_INFO: &str = "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable";
const CALLBACK_INFO_DESC: &str =
    "Lorg/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable;";

/// The driver the JVM runs: it loads the woven directory through a fresh
/// `URLClassLoader`, so no `demo` class can come from the application classpath.
const RUNNER: &str = r#"import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.nio.file.Paths;

class Runner {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(args[0]);
        String which = args[1];
        int x = args.length > 2 ? Integer.parseInt(args[2]) : 0;
        try (URLClassLoader loader =
                new URLClassLoader(new URL[] { dir.toUri().toURL() }, Runner.class.getClassLoader())) {
            Class<?> target = Class.forName("demo.Target", true, loader);
            Class<?> callback = Class.forName("demo.Callback", true, loader);
            if (which.equals("sites")) {
                Object instance = target.getConstructor().newInstance();
                target.getMethod("threeSites").invoke(instance);
            } else if (which.equals("pick")) {
                System.out.println("result=" + target.getMethod("pick", int.class).invoke(null, x));
            } else if (which.equals("twoReturns")) {
                System.out.println("result=" + target.getMethod("twoReturns", int.class).invoke(null, x));
            } else if (which.equals("twoCalls")) {
                target.getMethod("twoCalls").invoke(null);
            } else if (which.equals("twoCallsSlice")) {
                target.getMethod("twoCallsSlice").invoke(null);
            } else if (which.equals("twoCallsShift")) {
                target.getMethod("twoCallsShift").invoke(null);
            } else {
                throw new IllegalArgumentException(which);
            }
            System.out.println("calls=" + callback.getField("calls").getInt(null));
            System.out.println("seen=" + callback.getField("seen").get(null));
            System.out.println("value=" + callback.getField("value").get(null));
            System.out.println("trace=" + target.getField("trace").get(null));
        }
    }
}
"#;

fn jdk_tool(name: &str) -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin").join(name))
        .unwrap_or_else(|| name.into())
}

/// A fresh directory: tests run in parallel, so every call gets its own.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-selectors-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// Compile `demo/Target` and `demo/Callback` against the synthetic
/// `CallbackInfoReturnable` the weaver emits.
fn compile(name: &str) -> PathBuf {
    let root = scratch(name);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(src.join("org/spongepowered/asm/mixin/injection/callback")).unwrap();
    fs::write(src.join("demo/Target.java"), TARGET).unwrap();
    fs::write(src.join("demo/Callback.java"), CALLBACK).unwrap();
    let returnable = classes.join(format!("{CALLBACK_INFO}.class"));
    fs::create_dir_all(returnable.parent().unwrap()).unwrap();
    fs::write(&returnable, callback_info_returnable_class().unwrap()).unwrap();
    fs::create_dir_all(&classes).unwrap();
    let output = Command::new(jdk_tool("javac"))
        .arg("--release")
        .arg("17")
        .arg("-nowarn")
        .arg("-cp")
        .arg(&classes)
        .arg("-d")
        .arg(&classes)
        .arg(src.join("demo/Target.java"))
        .arg(src.join("demo/Callback.java"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "javac failed for {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

/// Write the woven class beside the fixture's other classes and run one method
/// in a fresh JVM.  Returns the runner's `key=value` lines.
fn run(classes: &Path, woven: &[u8], which: &str, argument: Option<&str>) -> Vec<String> {
    let root = scratch(&format!("run-{which}"));
    let dir = root.join("woven");
    fs::create_dir_all(dir.join("demo")).unwrap();
    fs::create_dir_all(dir.join("org/spongepowered/asm/mixin/injection/callback")).unwrap();
    for entry in fs::read_dir(classes.join("demo")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "class")
        {
            fs::copy(&path, dir.join("demo").join(path.file_name().unwrap())).unwrap();
        }
    }
    for entry in
        fs::read_dir(classes.join("org/spongepowered/asm/mixin/injection/callback")).unwrap()
    {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "class")
        {
            fs::copy(
                &path,
                dir.join("org/spongepowered/asm/mixin/injection/callback")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }
    fs::write(dir.join("demo/Target.class"), woven).unwrap();
    fs::write(root.join("Runner.java"), RUNNER).unwrap();
    let mut command = Command::new(jdk_tool("java"));
    command.arg(root.join("Runner.java")).arg(&dir).arg(which);
    if let Some(argument) = argument {
        command.arg(argument);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{which}: the woven class did not load or run:\n{}\n{stdout}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout.lines().map(str::to_owned).collect()
}

fn value<'a>(lines: &'a [String], key: &str) -> &'a str {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} line in {lines:?}"))
}

/// The `demo/Target.hit` selector with every overload.
fn every_hit(ordinal: Option<i32>, slice: Option<SliceBounds>) -> InvokeSelector {
    InvokeSelector {
        owner: "demo/Target".into(),
        name: "hit".into(),
        descriptor: String::new(),
        ordinal,
        slice,
    }
}

fn hit_on(descriptor: &str) -> SiteSelector {
    SiteSelector::Invoke {
        owner: "demo/Target".into(),
        name: "hit".into(),
        descriptor: descriptor.into(),
    }
}

/// The `demo/Target.hit` boundary that names the `ordinal`-th matching call.
fn hit_at(ordinal: i32) -> SiteSelector {
    SiteSelector::InvokeOrdinal {
        owner: "demo/Target".into(),
        name: "hit".into(),
        descriptor: String::new(),
        ordinal,
    }
}

/// The `ordinal`-th return-family boundary.
fn return_at(ordinal: i32) -> SiteSelector {
    SiteSelector::ReturnOrdinal { ordinal }
}

fn member(name: &str, descriptor: &str) -> MethodRef {
    method("demo/Target", name, descriptor)
}

/// The annotation stubs and one mixin whose `@At` selectors exercise
/// discovery.  `javac` encodes the enum `shift` and the nested `@Slice`
/// exactly like Mixin's own annotations, so the reader is exercised against
/// real class-file annotation bytes.
const DISCOVERY_SOURCES: &[(&str, &str)] = &[
    (
        "org/spongepowered/asm/mixin/Mixin.java",
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface Mixin {
    Class<?>[] value() default {};
    String[] targets() default {};
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/Slice.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface Slice {
    At from() default @At("HEAD");
    At to() default @At("TAIL");
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/At.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface At {
    String value() default "";
    String target() default "";
    String slice() default "";
    int ordinal() default -1;
    Shift shift() default Shift.NONE;
    int by() default 1;
    enum Shift { NONE, BEFORE, AFTER, BY }
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/Inject.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Inject {
    String[] method();
    Slice[] slice() default {};
    At[] at();
    boolean cancellable() default false;
}
"#,
    ),
    (
        "demo/SelectorMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.Slice;

@Mixin(targets = "Ldemo/MixinTarget;")
public abstract class SelectorMixin {
    @Inject(method = "twoReturns(I)I", at = @At(value = "RETURN", ordinal = 1))
    private void onReturn(int x) {}

    @Inject(method = "twoCalls()V", at = @At(value = "INVOKE", target = "Ldemo/MixinTarget;hit(I)V", ordinal = -1))
    private void onInvoke() {}

    @Inject(method = "twoCalls()V", at = @At(value = "INVOKE", target = "Ldemo/MixinTarget;hit(I)V"), slice = @Slice(from = @At(value = "INVOKE", target = "Ldemo/MixinTarget;hit(I)V"), to = @At("RETURN")))
    private void onSlice() {}

    @Inject(method = "twoCalls()V", at = @At(value = "INVOKE", target = "Ldemo/MixinTarget;hit(I)V", shift = At.Shift.AFTER, by = 1))
    private void onShift() {}

    @Inject(method = "twoCalls()V", at = @At(value = "RETURN", shift = At.Shift.AFTER))
    private void badShift() {}

    @Inject(method = "twoCalls()V", at = @At(value = "INVOKE", target = "Ldemo/MixinTarget;hit(I)V", shift = At.Shift.AFTER, by = 9))
    private void badBy() {}

    @Inject(method = "twoCalls()V", at = @At(value = "INVOKE", target = "Ldemo/MixinTarget;hit(I)V"), slice = @Slice(to = @At("RETURN")))
    private void badSlice() {}
}
"#,
    ),
];

/// Compile [`DISCOVERY_SOURCES`] and parse the mixin class, without needing a
/// jar: plan discovery only reads the mixin's own annotations.
fn compile_discovery(name: &str) -> NativeJar {
    let root = scratch(name);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let mut paths = Vec::new();
    for (relative, body) in DISCOVERY_SOURCES {
        let path = src.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        paths.push(path);
    }
    let output = Command::new(jdk_tool("javac"))
        .arg("--release")
        .arg("17")
        .arg("-nowarn")
        .arg("-d")
        .arg(&classes)
        .args(&paths)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "javac failed for {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = fs::read(classes.join("demo/SelectorMixin.class")).unwrap();
    NativeJar::from_classes([ClassFile::parse(&bytes).unwrap()])
}

// ---------------------------------------------------------------------------
// Class-file walking, so the structural assertions read the bytes the JVM
// would load instead of the weaver's own model.
// ---------------------------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn u1(&mut self) -> u8 {
        let byte = self.bytes[self.at];
        self.at += 1;
        byte
    }

    fn u2(&mut self) -> u16 {
        u16::from_be_bytes([self.u1(), self.u1()])
    }

    fn u4(&mut self) -> u32 {
        u32::from_be_bytes([self.u1(), self.u1(), self.u1(), self.u1()])
    }

    fn skip(&mut self, count: usize) {
        self.at += count;
    }

    fn bytes(&mut self, count: usize) -> &'a [u8] {
        let slice = &self.bytes[self.at..self.at + count];
        self.at += count;
        slice
    }
}

/// One method's `Code` attribute as the class file records it.
struct CodeInfo {
    code: Vec<u8>,
    /// `StackMapTable` frame offsets.
    frames: Vec<usize>,
    /// `(start_pc, end_pc, handler_pc, catch_type)` rows.
    exception_table: Vec<[u16; 4]>,
}

fn method_code(bytes: &[u8], name: &str, descriptor: &str) -> CodeInfo {
    let mut r = Reader::new(bytes);
    assert_eq!(r.u4(), 0xCAFE_BABE);
    r.skip(4);
    let count = r.u2() as usize;
    let mut utf8: Vec<Option<String>> = vec![None; count];
    let mut at = 1usize;
    while at < count {
        let tag = r.u1();
        match tag {
            1 => {
                let length = r.u2() as usize;
                utf8[at] = Some(String::from_utf8(r.bytes(length).to_vec()).unwrap());
            }
            3 | 4 => r.skip(4),
            5 | 6 => {
                r.skip(8);
                at += 1;
            }
            7 | 8 | 16 | 19 | 20 => r.skip(2),
            9 | 10 | 11 | 12 | 17 | 18 => r.skip(4),
            15 => r.skip(3),
            other => panic!("unexpected constant-pool tag {other}"),
        }
        at += 1;
    }
    r.skip(2); // access_flags
    r.skip(2); // this_class
    r.skip(2); // super_class
    let interfaces = r.u2() as usize;
    r.skip(interfaces * 2);
    let fields = r.u2() as usize;
    for _ in 0..fields {
        r.skip(6);
        skip_attributes(&mut r);
    }
    let methods = r.u2() as usize;
    for _ in 0..methods {
        r.skip(2); // access_flags
        let name_index = r.u2() as usize;
        let descriptor_index = r.u2() as usize;
        let attributes = r.u2() as usize;
        let matches = utf8[name_index].as_deref() == Some(name)
            && utf8[descriptor_index].as_deref() == Some(descriptor);
        let mut found = None;
        for _ in 0..attributes {
            let attribute_name = r.u2() as usize;
            let length = r.u4() as usize;
            let body = r.bytes(length);
            if matches && utf8[attribute_name].as_deref() == Some("Code") {
                found = Some(read_code(body, &utf8));
            }
        }
        if let Some(found) = found {
            return found;
        }
    }
    panic!("no {name}{descriptor} in the class file");
}

fn skip_attributes(r: &mut Reader<'_>) {
    let count = r.u2() as usize;
    for _ in 0..count {
        r.skip(2);
        let length = r.u4() as usize;
        r.skip(length);
    }
}

fn read_code(body: &[u8], utf8: &[Option<String>]) -> CodeInfo {
    let mut r = Reader::new(body);
    r.skip(2); // max_stack
    r.skip(2); // max_locals
    let length = r.u4() as usize;
    let code = r.bytes(length).to_vec();
    let handlers = r.u2() as usize;
    let mut exception_table = Vec::with_capacity(handlers);
    for _ in 0..handlers {
        exception_table.push([r.u2(), r.u2(), r.u2(), r.u2()]);
    }
    let attributes = r.u2() as usize;
    let mut frames = Vec::new();
    for _ in 0..attributes {
        let name_index = r.u2() as usize;
        let length = r.u4() as usize;
        let body = r.bytes(length);
        if utf8[name_index].as_deref() == Some("StackMapTable") {
            frames = read_frames(body);
        }
    }
    CodeInfo {
        code,
        frames,
        exception_table,
    }
}

fn read_frames(body: &[u8]) -> Vec<usize> {
    let mut r = Reader::new(body);
    let count = r.u2() as usize;
    let mut frames: Vec<usize> = Vec::with_capacity(count);
    for _ in 0..count {
        let frame_type = r.u1();
        let delta = match frame_type {
            0..=63 => frame_type as usize,
            64..=127 => {
                skip_verifications(&mut r, 1);
                frame_type as usize - 64
            }
            247 => {
                let delta = r.u2() as usize;
                skip_verifications(&mut r, 1);
                delta
            }
            248..=251 => r.u2() as usize,
            252..=254 => {
                let delta = r.u2() as usize;
                skip_verifications(&mut r, frame_type as usize - 251);
                delta
            }
            _ => {
                let delta = r.u2() as usize;
                let locals = r.u2() as usize;
                skip_verifications(&mut r, locals);
                let stack = r.u2() as usize;
                skip_verifications(&mut r, stack);
                delta
            }
        };
        let offset = match frames.last() {
            Some(previous) => previous + delta + 1,
            None => delta,
        };
        frames.push(offset);
    }
    frames
}

fn skip_verifications(r: &mut Reader<'_>, count: usize) {
    for _ in 0..count {
        let tag = r.u1();
        if tag == 7 || tag == 8 {
            r.skip(2);
        }
    }
}

/// The `(offset, size, opcode)` of every instruction, so a shifted insert is
/// expected from the fixture's own layout instead of a hard-coded offset.
fn scan(code: &[u8]) -> Vec<(usize, usize, u8)> {
    let mut instructions = Vec::new();
    let mut at = 0usize;
    while at < code.len() {
        let opcode = code[at];
        let size = match opcode {
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
            other => panic!("unexpected opcode 0x{other:02x} in the fixture"),
        };
        instructions.push((at, size, opcode));
        at += size;
    }
    instructions
}

/// Assert the one insert sits at `at`: the prefix is untouched and the original
/// tail is shifted by the insert's length.  Returns the inserted bytes.
fn assert_insert_at(original: &[u8], woven: &[u8], at: usize) -> Vec<u8> {
    let insert = woven.len() - original.len();
    assert!(insert > 0, "the weave inserted nothing");
    assert_eq!(&woven[..at], &original[..at], "the prefix before {at}");
    assert_eq!(
        &woven[at + insert..],
        &original[at..],
        "the original tail is shifted by the {insert}-byte insert at {at}"
    );
    woven[at..at + insert].to_vec()
}

/// Assert an insert of `length` bytes starts at `at` and the original
/// instruction follows it.  A branch that named an instruction past `at`
/// legitimately has its operand relocated, so unlike [`assert_insert_at`] this
/// does not require the prefix to be byte-identical.  Returns the inserted
/// bytes.
fn assert_insert_before(original: &[u8], woven: &[u8], at: usize, length: usize) -> Vec<u8> {
    assert_eq!(
        woven.len(),
        original.len() + length,
        "one {length}-byte insert"
    );
    assert_eq!(
        woven[at + length],
        original[at],
        "the original instruction at {at} follows the insert"
    );
    let boundaries = scan(woven);
    assert!(
        boundaries.iter().any(|(offset, _, _)| *offset == at),
        "the insert starts at the instruction boundary {at}"
    );
    assert!(
        boundaries
            .iter()
            .any(|(offset, _, _)| *offset == at + length),
        "the original instruction stays a boundary at {}",
        at + length
    );
    woven[at..at + length].to_vec()
}

// ---------------------------------------------------------------------------
// The hand-built invoke target: one call site and one exception range.
// ---------------------------------------------------------------------------

/// Pool: 1 `demo/T`, 2 Class 1, 3 `java/lang/Object`, 4 Class 3, 5 `m`,
/// 6 `()V`, 7 `Code`, 8 `a`, 9 `demo/U`, 10 Class 9, 11 `a ()V`,
/// 12 MethodRef 10.11.
fn guarded_class() -> Vec<u8> {
    fn u2(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn u4(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn utf8(out: &mut Vec<u8>, value: &str) {
        out.push(1);
        u2(out, value.len() as u16);
        out.extend_from_slice(value.as_bytes());
    }
    let mut out = Vec::new();
    u4(&mut out, 0xCAFE_BABE);
    u2(&mut out, 0);
    u2(&mut out, 52);
    u2(&mut out, 13);
    utf8(&mut out, "demo/T");
    out.extend_from_slice(&[7, 0, 1]);
    utf8(&mut out, "java/lang/Object");
    out.extend_from_slice(&[7, 0, 3]);
    utf8(&mut out, "m");
    utf8(&mut out, "()V");
    utf8(&mut out, "Code");
    utf8(&mut out, "a");
    utf8(&mut out, "demo/U");
    out.extend_from_slice(&[7, 0, 9]);
    out.extend_from_slice(&[12, 0, 8, 0, 6]);
    out.extend_from_slice(&[10, 0, 10, 0, 11]);
    u2(&mut out, 0x0021); // access
    u2(&mut out, 2); // this
    u2(&mut out, 4); // super
    u2(&mut out, 0); // interfaces
    u2(&mut out, 0); // fields
    u2(&mut out, 1); // methods
    u2(&mut out, 0x0009); // public static
    u2(&mut out, 5);
    u2(&mut out, 6);
    u2(&mut out, 1); // attributes
    u2(&mut out, 7);
    // 0: invokestatic demo/U.a()V; 3: nop; 4: pop; 5: return
    let code = [0xb8, 0x00, 0x0c, 0x00, 0x57, 0xb1];
    let mut body = Vec::new();
    u2(&mut body, 1); // max_stack
    u2(&mut body, 0); // max_locals
    u4(&mut body, code.len() as u32);
    body.extend_from_slice(&code);
    u2(&mut body, 1); // exception table
    u2(&mut body, 0); // start_pc
    u2(&mut body, code.len() as u16); // end_pc
    u2(&mut body, 4); // handler_pc
    u2(&mut body, 0); // catch-all
    u2(&mut body, 0); // Code attributes
    u4(&mut out, body.len() as u32);
    out.extend_from_slice(&body);
    u2(&mut out, 0); // class attributes
    out
}

/// `public static int demo/T.m(int)` whose `ireturn` is a `goto` target and
/// therefore carries a `StackMapTable` frame:
///
/// ```text
/// 0: iload_0
/// 1: ifeq 8
/// 4: iconst_1
/// 5: goto 9
/// 8: iconst_2       frame at 8
/// 9: ireturn        frame at 9
/// ```
///
/// Pool: 1 `demo/T`, 2 Class 1, 3 `java/lang/Object`, 4 Class 3, 5 `m`,
/// 6 `(I)I`, 7 `Code`, 8 `StackMapTable`.
fn two_arm_class() -> Vec<u8> {
    fn u2(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn u4(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn utf8(out: &mut Vec<u8>, value: &str) {
        out.push(1);
        u2(out, value.len() as u16);
        out.extend_from_slice(value.as_bytes());
    }
    let mut out = Vec::new();
    u4(&mut out, 0xCAFE_BABE);
    u2(&mut out, 0);
    u2(&mut out, 52);
    u2(&mut out, 9);
    utf8(&mut out, "demo/T");
    out.extend_from_slice(&[7, 0, 1]);
    utf8(&mut out, "java/lang/Object");
    out.extend_from_slice(&[7, 0, 3]);
    utf8(&mut out, "m");
    utf8(&mut out, "(I)I");
    utf8(&mut out, "Code");
    utf8(&mut out, "StackMapTable");
    u2(&mut out, 0x0021); // access
    u2(&mut out, 2); // this
    u2(&mut out, 4); // super
    u2(&mut out, 0); // interfaces
    u2(&mut out, 0); // fields
    u2(&mut out, 1); // methods
    u2(&mut out, 0x0009); // public static
    u2(&mut out, 5);
    u2(&mut out, 6);
    u2(&mut out, 1); // method attributes
    u2(&mut out, 7);
    let code = [
        0x1a, // iload_0
        0x99, 0x00, 0x07, // ifeq 8
        0x04, // iconst_1
        0xa7, 0x00, 0x04, // goto 9
        0x05, // iconst_2
        0xac, // ireturn
    ];
    let mut body = Vec::new();
    u2(&mut body, 1); // max_stack
    u2(&mut body, 1); // max_locals
    u4(&mut body, code.len() as u32);
    body.extend_from_slice(&code);
    u2(&mut body, 0); // exception table
    u2(&mut body, 1); // Code attributes
    u2(&mut body, 8); // StackMapTable
    let frames = [0x00, 0x02, 8, 64, 1];
    u4(&mut body, frames.len() as u32);
    body.extend_from_slice(&frames);
    u4(&mut out, body.len() as u32);
    out.extend_from_slice(&body);
    u2(&mut out, 0); // class attributes
    out
}

// ---------------------------------------------------------------------------
// INVOKE ordinal
// ---------------------------------------------------------------------------

#[test]
fn invoke_ordinal_selects_each_of_the_three_sites() {
    let classes = compile("ordinal");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = member("threeSites", "()V");
    let on_call = callback("onCall", "()V");

    // `weave_invoke` keeps every matching site: the three overloads all run the
    // callback, in file order, so the last call sees both earlier bodies.
    let every = weave_invoke(
        &class,
        &target,
        &on_call,
        &method("demo/Target", "hit", ""),
        false,
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&every).unwrap().write().unwrap(),
        every,
        "the woven class round-trips"
    );
    let lines = run(&classes, &every, "sites", None);
    assert_eq!(value(&lines, "calls="), "3");
    assert_eq!(value(&lines, "seen="), "is", "the last site runs last");
    assert_eq!(value(&lines, "trace="), "isv");

    // Ordinal 0 is the first `invokestatic`, 1 the second, 2 and -1 the
    // `invokevirtual`.  `seen` is `Target.trace` at the moment the callback ran,
    // so it names the site exactly.
    for (ordinal, seen) in [
        (Some(0), ""),
        (Some(1), "i"),
        (Some(2), "is"),
        (Some(-1), "is"),
    ] {
        let selector = every_hit(ordinal, None);
        let woven = weave_invoke_selected(&class, &target, &on_call, &selector, false).unwrap();
        assert_eq!(
            ClassFile::parse(&woven).unwrap().write().unwrap(),
            woven,
            "ordinal {ordinal:?} round-trips"
        );
        let lines = run(&classes, &woven, "sites", None);
        assert_eq!(value(&lines, "calls="), "1", "ordinal {ordinal:?}");
        assert_eq!(value(&lines, "seen="), seen, "ordinal {ordinal:?}");
        assert_eq!(value(&lines, "trace="), "isv", "ordinal {ordinal:?}");
        // The narrowed entry point has to produce the same class.
        assert_eq!(
            weave_invoke_at(
                &class,
                &target,
                &on_call,
                &method("demo/Target", "hit", ""),
                ordinal,
                false,
            )
            .unwrap(),
            woven,
            "weave_invoke_at({ordinal:?})"
        );
    }

    // A negative ordinal counts from the end, not from the first site.
    let woven = weave_invoke_selected(&class, &target, &on_call, &every_hit(Some(-2), None), false)
        .unwrap();
    let lines = run(&classes, &woven, "sites", None);
    assert_eq!(value(&lines, "seen="), "i", "-2 is the middle site");

    for ordinal in [3, -4, i32::MIN] {
        let error = weave_invoke_selected(
            &class,
            &target,
            &on_call,
            &every_hit(Some(ordinal), None),
            false,
        )
        .unwrap_err();
        assert!(error.contains("ordinal"), "{error}");
        assert!(error.contains("3 matching"), "{error}");
    }
}

// ---------------------------------------------------------------------------
// INVOKE slice
// ---------------------------------------------------------------------------

#[test]
fn invoke_slice_counts_only_the_sites_between_its_boundaries() {
    let classes = compile("slice");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = member("threeSites", "()V");
    let on_call = callback("onCall", "()V");

    // `[hit(I)V, hit(J)V)` holds the two `invokestatic` sites, so ordinal 1 is
    // the middle call and ordinal 0 the first; the virtual site is outside.
    let slice = SliceBounds {
        from: hit_on("(I)V"),
        to: hit_on("(J)V"),
    };
    let middle = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(Some(1), Some(slice.clone())),
        false,
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&middle).unwrap().write().unwrap(),
        middle,
        "the woven class round-trips"
    );
    let lines = run(&classes, &middle, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(
        value(&lines, "seen="),
        "i",
        "the middle site is the String call"
    );
    assert_eq!(value(&lines, "trace="), "isv");

    let first = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), Some(slice.clone())),
        false,
    )
    .unwrap();
    let lines = run(&classes, &first, "sites", None);
    assert_eq!(value(&lines, "seen="), "");

    // Two sites inside the slice: ordinal 2 is out of range even though the
    // method has three matching sites.
    let error = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(Some(2), Some(slice)),
        false,
    )
    .unwrap_err();
    assert!(error.contains("ordinal 2"), "{error}");
    assert!(error.contains("2 matching"), "{error}");

    // A slice whose boundaries leave out every call site holds no call site at
    // all: `[HEAD, hit(I)V)` stops before the first call.
    let empty = SliceBounds {
        from: SiteSelector::Head,
        to: hit_on("(I)V"),
    };
    let error = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(empty)),
        false,
    )
    .unwrap_err();
    assert!(error.contains("found no"), "{error}");
    assert!(error.contains("inside the slice"), "{error}");

    // A boundary that matches more than one instruction does not name a slice
    // edge: the descriptor-less selector matches all three sites.
    let ambiguous = SliceBounds {
        from: hit_on(""),
        to: SiteSelector::Tail,
    };
    let error = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(ambiguous)),
        false,
    )
    .unwrap_err();
    assert!(error.contains("matches 3 instructions"), "{error}");

    // A slice that ends before it starts, and one with nothing between its
    // boundaries, are both reasons.
    let inverted = SliceBounds {
        from: SiteSelector::Tail,
        to: SiteSelector::Head,
    };
    let error = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(inverted)),
        false,
    )
    .unwrap_err();
    assert!(error.contains("ends before it starts"), "{error}");

    let empty = SliceBounds {
        from: SiteSelector::Head,
        to: SiteSelector::Head,
    };
    let error = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(empty)),
        false,
    )
    .unwrap_err();
    assert!(error.contains("contains no instruction"), "{error}");
}

// ---------------------------------------------------------------------------
// INVOKE shift / by
// ---------------------------------------------------------------------------

#[test]
fn invoke_shift_moves_the_insert_to_an_instruction_boundary() {
    let classes = compile("shift");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = member("threeSites", "()V");
    let on_call = callback("onCall", "()V");
    let original = class
        .method("threeSites", "()V")
        .unwrap()
        .code
        .clone()
        .unwrap();
    let instructions = scan(&original);
    let sites = instructions
        .iter()
        .enumerate()
        .filter(|(_, (_, _, opcode))| matches!(opcode, 0xb6..=0xb9))
        .map(|(index, (at, _, _))| (index, *at))
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 3, "the fixture has three call sites");
    let first = sites[0].0;
    assert_eq!(
        instructions[first].2, 0xb8,
        "the first site is invokestatic"
    );

    // `Before by 0` is the site itself and has to agree with the plain weave.
    let unshifted =
        weave_invoke_selected(&class, &target, &on_call, &every_hit(Some(0), None), false).unwrap();
    let shifted = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        InvokeShift {
            direction: Shift::Before,
            by: 0,
        },
        false,
    )
    .unwrap();
    assert_eq!(shifted, unshifted, "Before by 0 is the selected site");
    let (code, _, _) = woven_code(&shifted, "threeSites", "()V");
    let insert = assert_insert_at(&original, &code, sites[0].1);
    assert_eq!(insert.len(), 3, "invokestatic Callback.onCall");
    assert_eq!(insert[0], 0xb8, "the callback call is an invokestatic");

    // One instruction earlier: the `iconst_1` that pushes the argument.
    let before = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        InvokeShift {
            direction: Shift::Before,
            by: 1,
        },
        false,
    )
    .unwrap();
    let (code, _, _) = woven_code(&before, "threeSites", "()V");
    assert_insert_at(&original, &code, instructions[first - 1].0);

    // `After by 0` is the instruction after the call, `After by 1` the one after
    // that.
    for (by, expected) in [
        (0usize, instructions[first + 1].0),
        (1, instructions[first + 2].0),
    ] {
        let after = weave_invoke_shifted(
            &class,
            &target,
            &on_call,
            &every_hit(Some(0), None),
            InvokeShift {
                direction: Shift::After,
                by: by as u8,
            },
            false,
        )
        .unwrap();
        let (code, _, _) = woven_code(&after, "threeSites", "()V");
        assert_insert_at(&original, &code, expected);
    }

    // After the virtual call, the callback sees the third body already ran:
    // `seen` is `isv`, not the `is` the unshifted insert sees.
    let after_last = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(-1), None),
        InvokeShift {
            direction: Shift::After,
            by: 0,
        },
        false,
    )
    .unwrap();
    let (code, _, _) = woven_code(&after_last, "threeSites", "()V");
    assert_insert_at(&original, &code, instructions[sites[2].0 + 1].0);
    let lines = run(&classes, &after_last, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(
        value(&lines, "seen="),
        "isv",
        "the call ran before the callback"
    );

    // The bound, the two ends of the method, and cancellation are reasons.
    let too_far = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        InvokeShift {
            direction: Shift::Before,
            by: 9,
        },
        false,
    )
    .unwrap_err();
    assert!(too_far.contains("bounded shift of 0..=8"), "{too_far}");

    let past_end = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(-1), None),
        InvokeShift {
            direction: Shift::After,
            by: 1,
        },
        false,
    )
    .unwrap_err();
    assert!(
        past_end.contains("runs past the method's last instruction"),
        "{past_end}"
    );

    let past_start = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        InvokeShift {
            direction: Shift::Before,
            by: 2,
        },
        false,
    )
    .unwrap_err();
    assert!(
        past_start.contains("runs past the method's first instruction"),
        "{past_start}"
    );

    let cancellable = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        InvokeShift {
            direction: Shift::After,
            by: 0,
        },
        true,
    )
    .unwrap_err();
    assert!(cancellable.contains("cannot be shifted"), "{cancellable}");
}

#[test]
fn a_shifted_insert_cannot_split_an_exception_range() {
    let class = ClassFile::parse(&guarded_class()).unwrap();
    let target = method("demo/T", "m", "()V");
    let on_call = callback("onCall", "()V");
    let selector = InvokeSelector {
        owner: "demo/U".into(),
        name: "a".into(),
        descriptor: "()V".into(),
        ordinal: None,
        slice: None,
    };

    // The call is the range start, where a plain insert lands before the range;
    // one instruction later is inside it, so the target's handler would catch
    // the callback.
    let woven = weave_invoke_selected(&class, &target, &on_call, &selector, false).unwrap();
    let info = method_code(&woven, "m", "()V");
    assert_eq!(info.code.len(), 6 + 3, "one 3-byte invokestatic");
    assert_eq!(
        info.exception_table,
        vec![[3, 9, 7, 0]],
        "the callback stays outside [3, 9), the handler moved to 7"
    );

    let error = weave_invoke_shifted(
        &class,
        &target,
        &on_call,
        &selector,
        InvokeShift {
            direction: Shift::After,
            by: 0,
        },
        false,
    )
    .unwrap_err();
    assert!(error.contains("exception range"), "{error}");
    assert!(error.contains("[0, 6)"), "{error}");

    // Shift.BY names the same signed distance: +1 is the instruction after the
    // call, which is inside the guarded range.
    let error = weave_invoke_shifted_by(&class, &target, &on_call, &selector, Shift::By(1), false)
        .unwrap_err();
    assert!(error.contains("exception range"), "{error}");
    assert!(error.contains("[0, 6)"), "{error}");
}

// ---------------------------------------------------------------------------
// Shift.BY: a signed instruction distance
// ---------------------------------------------------------------------------

#[test]
fn shift_by_moves_either_way_and_runs() {
    let classes = compile("shift-by");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = member("threeSites", "()V");
    let on_call = callback("onCall", "()V");
    let original = class
        .method("threeSites", "()V")
        .unwrap()
        .code
        .clone()
        .unwrap();
    let instructions = scan(&original);
    let sites = instructions
        .iter()
        .enumerate()
        .filter(|(_, (_, _, opcode))| matches!(opcode, 0xb6..=0xb9))
        .map(|(index, (at, _, _))| (index, *at))
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 3, "the fixture has three call sites");
    let first = sites[0].0;

    // `BY 0` is the call itself, byte for byte the unshifted weave.
    let unshifted =
        weave_invoke_selected(&class, &target, &on_call, &every_hit(Some(0), None), false).unwrap();
    let zero = weave_invoke_shifted_by(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        Shift::By(0),
        false,
    )
    .unwrap();
    assert_eq!(zero, unshifted, "Shift.BY 0 names the selected call");

    // Negative moves toward the start: -1 is the `iconst_1` that pushes the
    // call's argument, so the callback runs before the first `hit`.
    let negative = weave_invoke_shifted_by(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        Shift::By(-1),
        false,
    )
    .unwrap();
    let (code, _, _) = woven_code(&negative, "threeSites", "()V");
    assert_insert_at(&original, &code, instructions[first - 1].0);
    let lines = run(&classes, &negative, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(
        value(&lines, "seen="),
        "",
        "the callback ran before the first hit"
    );
    assert_eq!(value(&lines, "trace="), "isv");

    // Positive moves toward the end: +1 is the instruction after the call, so
    // the first `hit` already ran, and +2 the one after that.
    for (by, index) in [(1i32, first + 1), (2, first + 2)] {
        let shifted = weave_invoke_shifted_by(
            &class,
            &target,
            &on_call,
            &every_hit(Some(0), None),
            Shift::By(by),
            false,
        )
        .unwrap();
        let (code, _, _) = woven_code(&shifted, "threeSites", "()V");
        assert_insert_at(&original, &code, instructions[index].0);
    }
    let positive = weave_invoke_shifted_by(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        Shift::By(1),
        false,
    )
    .unwrap();
    let lines = run(&classes, &positive, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(
        value(&lines, "seen="),
        "i",
        "the first hit ran before the callback"
    );
    assert_eq!(value(&lines, "trace="), "isv");

    // The signed bound and both ends of the method are reasons.
    for by in [9, -9, i32::MAX, i32::MIN] {
        let error = weave_invoke_shifted_by(
            &class,
            &target,
            &on_call,
            &every_hit(Some(0), None),
            Shift::By(by),
            false,
        )
        .unwrap_err();
        assert!(error.contains("bounded shift of -8..=8"), "{error}");
    }
    let error = weave_invoke_shifted_by(
        &class,
        &target,
        &on_call,
        &every_hit(Some(-1), None),
        Shift::By(2),
        false,
    )
    .unwrap_err();
    assert!(
        error.contains("runs past the method's last instruction"),
        "{error}"
    );
    let error = weave_invoke_shifted_by(
        &class,
        &target,
        &on_call,
        &every_hit(Some(0), None),
        Shift::By(-3),
        false,
    )
    .unwrap_err();
    assert!(
        error.contains("runs past the method's first instruction"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// RETURN / TAIL / HEAD ordinals
// ---------------------------------------------------------------------------

#[test]
fn returns_ordinal_selects_one_return_and_observes_its_value() {
    let classes = compile("returns");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = member("pick", "(I)I");
    let on_return = callback("onReturn", &format!("(I{CALLBACK_INFO_DESC})V"));
    let selector = |mode, ordinal| ReturnSelector {
        mode,
        ordinal,
        slice: None,
    };

    // `pick(10)` takes the first return, `pick(-1)` the third.
    let first = weave_returns_selected(
        &class,
        &target,
        &on_return,
        true,
        &selector(ReturnMode::Return, Some(0)),
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&first).unwrap().write().unwrap(),
        first,
        "the woven class round-trips"
    );
    let lines = run(&classes, &first, "pick", Some("10"));
    assert_eq!(value(&lines, "result="), "1");
    assert_eq!(
        value(&lines, "calls="),
        "1",
        "only the first return callback ran"
    );
    assert_eq!(value(&lines, "value="), "1");

    // The same weave sees no callback on the path that returns through another
    // arm.
    let lines = run(&classes, &first, "pick", Some("-1"));
    assert_eq!(value(&lines, "result="), "3");
    assert_eq!(value(&lines, "calls="), "0");
    assert_eq!(value(&lines, "value="), "null");

    let last = weave_returns_selected(
        &class,
        &target,
        &on_return,
        true,
        &selector(ReturnMode::Return, Some(-1)),
    )
    .unwrap();
    let lines = run(&classes, &last, "pick", Some("-1"));
    assert_eq!(value(&lines, "result="), "3");
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "value="), "3");
    let lines = run(&classes, &last, "pick", Some("10"));
    assert_eq!(value(&lines, "calls="), "0");

    // TAIL keeps the last return whatever the ordinal counts from: 0 and -1 both
    // name it, and anything else is out of range.
    for ordinal in [Some(0), Some(-1), None] {
        let tail = weave_returns_selected(
            &class,
            &target,
            &on_return,
            true,
            &selector(ReturnMode::Tail, ordinal),
        )
        .unwrap();
        assert_eq!(tail, last, "TAIL {ordinal:?} is the last return");
    }
    let error = weave_returns_selected(
        &class,
        &target,
        &on_return,
        true,
        &selector(ReturnMode::Tail, Some(1)),
    )
    .unwrap_err();
    assert!(error.contains("ordinal 1"), "{error}");
    assert!(error.contains("1 matching"), "{error}");

    // Ordinal 3 and -4 are outside the three returns; the message is the same
    // bounded one the invoke selector uses.
    for ordinal in [3, -4] {
        let error = weave_returns_selected(
            &class,
            &target,
            &on_return,
            true,
            &selector(ReturnMode::Return, Some(ordinal)),
        )
        .unwrap_err();
        assert!(error.contains("ordinal"), "{error}");
        assert!(error.contains("3 matching"), "{error}");
    }

    // `[RETURN, TAIL)` starts at the first return and stops before the last, so
    // it counts two of the three: ordinal 0 is the first, and the third return
    // never calls back.
    let slice = SliceBounds {
        from: SiteSelector::Return,
        to: SiteSelector::Tail,
    };
    let sliced = weave_returns_selected(
        &class,
        &target,
        &on_return,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: Some(0),
            slice: Some(slice),
        },
    )
    .unwrap();
    assert_eq!(
        value(&run(&classes, &sliced, "pick", Some("10")), "value="),
        "1"
    );
    assert_eq!(
        value(&run(&classes, &sliced, "pick", Some("-1")), "calls="),
        "0",
        "the last return is outside the slice"
    );
}

#[test]
fn slice_boundaries_accept_ordinals() {
    let classes = compile("boundary-ordinals");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let on_return = callback("onReturn", &format!("(I{CALLBACK_INFO_DESC})V"));
    let on_call = callback("onCall", "()V");

    // `pick` has three returns; `[RETURN 1, TAIL)` holds only the second, so
    // `pick(1)` (which returns 2) is the only path that calls back.
    let second = SliceBounds {
        from: return_at(1),
        to: SiteSelector::Tail,
    };
    let woven = weave_returns_selected(
        &class,
        &member("pick", "(I)I"),
        &on_return,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: None,
            slice: Some(second),
        },
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the woven class round-trips"
    );
    let lines = run(&classes, &woven, "pick", Some("1"));
    assert_eq!(value(&lines, "result="), "2");
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "value="), "2");
    for argument in ["10", "-1"] {
        let lines = run(&classes, &woven, "pick", Some(argument));
        assert_eq!(
            value(&lines, "calls="),
            "0",
            "pick({argument}) returns through a different arm"
        );
    }

    // The same boundary counted from the end: -2 is the second of three.
    let second_from_end = SliceBounds {
        from: return_at(-2),
        to: SiteSelector::Tail,
    };
    let same = weave_returns_selected(
        &class,
        &member("pick", "(I)I"),
        &on_return,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: None,
            slice: Some(second_from_end),
        },
    )
    .unwrap();
    assert_eq!(same, woven, "RETURN -2 is the ordinal-1 boundary");

    // `threeSites` has three matching calls.  A boundary ordinal is resolved
    // against all of them before the slice is applied: `[HEAD, INVOKE 1)` holds
    // the first call, and `[INVOKE 1, INVOKE -1)` the middle one.
    let target = member("threeSites", "()V");
    let first_slice = SliceBounds {
        from: SiteSelector::Head,
        to: hit_at(1),
    };
    let first = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(first_slice)),
        false,
    )
    .unwrap();
    let lines = run(&classes, &first, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "", "the first hit is selected");

    let middle_slice = SliceBounds {
        from: hit_at(1),
        to: hit_at(-1),
    };
    let middle = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(middle_slice)),
        false,
    )
    .unwrap();
    let lines = run(&classes, &middle, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "i", "the middle hit is selected");

    // A boundary ordinal outside its matches names the count instead of
    // guessing an edge.
    let bad_return = SliceBounds {
        from: SiteSelector::Head,
        to: return_at(3),
    };
    let error = weave_returns_selected(
        &class,
        &member("pick", "(I)I"),
        &on_return,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: None,
            slice: Some(bad_return),
        },
    )
    .unwrap_err();
    assert!(error.contains("ordinal 3"), "{error}");
    assert!(error.contains("3 matching"), "{error}");

    let bad_invoke = SliceBounds {
        from: SiteSelector::Head,
        to: hit_at(-4),
    };
    let error = weave_invoke_selected(
        &class,
        &target,
        &on_call,
        &every_hit(None, Some(bad_invoke)),
        false,
    )
    .unwrap_err();
    assert!(error.contains("ordinal -4"), "{error}");
    assert!(error.contains("3 matching"), "{error}");
}

#[test]
fn selected_returns_keep_their_frames() {
    let bytes = two_arm_class();
    let class = ClassFile::parse(&bytes).unwrap();
    let target = method("demo/T", "m", "(I)I");
    let returnable = callback("onReturn", &format!("(I{CALLBACK_INFO_DESC})V"));
    let plain = callback("onPlain", "(I)V");
    let original = method_code(&bytes, "m", "(I)I");
    assert_eq!(original.code.len(), 10);
    assert_eq!(original.frames, vec![8, 9]);

    // Non-void with `non_void = true`: the insert owns the frame of the return
    // it replaced, because it stores the boxed value in a fresh local.
    let woven = weave_returns_selected(
        &class,
        &target,
        &returnable,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: Some(-1),
            slice: None,
        },
    )
    .unwrap();
    let info = method_code(&woven, "m", "(I)I");
    let insert = assert_insert_at(&original.code, &info.code, 9);
    // valueOf, astore, new, dup, aload, <init>, astore, iload_0, aload,
    // invokestatic, aload, getReturnValue, checkcast, intValue.
    assert_eq!(
        insert.len(),
        34,
        "the box/construct/call/read-back sequence"
    );
    assert_eq!(insert[0], 0xb8, "Integer.valueOf first");
    assert_eq!(
        info.frames,
        vec![8, 9],
        "the insert keeps the frame, the return behind it starts no block"
    );

    // `non_void = false` on a value-returning target: the insert is
    // stack-neutral, so the frame at the return still describes both offsets.
    let woven = weave_returns_selected(
        &class,
        &target,
        &plain,
        false,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: Some(-1),
            slice: None,
        },
    )
    .unwrap();
    let info = method_code(&woven, "m", "(I)I");
    let insert = assert_insert_at(&original.code, &info.code, 9);
    assert_eq!(insert.len(), 5, "iload_0 then invokestatic");
    assert_eq!(info.frames, vec![8, 9, 9 + insert.len()]);

    // The plain shape is the one a non-void return accepts, and the JVM takes
    // it: `pick(10)` still answers 1 with the callback run first.
    let classes = compile("returns-plain");
    let pick = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let woven = weave_returns_selected(
        &pick,
        &member("pick", "(I)I"),
        &plain,
        false,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: Some(0),
            slice: None,
        },
    )
    .unwrap();
    let lines = run(&classes, &woven, "pick", Some("10"));
    assert_eq!(value(&lines, "result="), "1");
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "plain");

    // `non_void = true` demands the returnable shape: a plain callback cannot
    // carry the result.
    let error = weave_returns_selected(
        &class,
        &target,
        &plain,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: None,
            slice: None,
        },
    )
    .unwrap_err();
    assert!(error.contains("CallbackInfoReturnable"), "{error}");

    // `non_void = true` on a `void` target has nothing to observe.
    let void = ClassFile::parse(&guarded_class()).unwrap();
    let error = weave_returns_selected(
        &void,
        &method("demo/T", "m", "()V"),
        &returnable,
        true,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: None,
            slice: None,
        },
    )
    .unwrap_err();
    assert!(error.contains("answers void"), "{error}");
}

#[test]
fn head_selected_counts_the_single_head_site() {
    let bytes = two_arm_class();
    let class = ClassFile::parse(&bytes).unwrap();
    let target = method("demo/T", "m", "(I)I");
    let plain = callback("onPlain", "(I)V");
    let original = method_code(&bytes, "m", "(I)I").code;

    // `None`, 0 and -1 all name offset 0, and the two frames move with the
    // 5-byte insert (`iload_0; invokestatic`).
    let mut expected: Option<Vec<u8>> = None;
    for ordinal in [None, Some(0), Some(-1)] {
        let woven = weave_head_selected(&class, &target, &plain, ordinal).unwrap();
        assert_eq!(
            ClassFile::parse(&woven).unwrap().write().unwrap(),
            woven,
            "HEAD {ordinal:?} round-trips"
        );
        let info = method_code(&woven, "m", "(I)I");
        let insert = assert_insert_at(&original, &info.code, 0);
        assert_eq!(insert.len(), 5, "iload_0 then invokestatic");
        assert_eq!(info.frames, vec![8 + 5, 9 + 5]);
        match &expected {
            Some(expected) => assert_eq!(&woven, expected),
            None => expected = Some(woven),
        }
    }

    for ordinal in [1, -2] {
        let error = weave_head_selected(&class, &target, &plain, Some(ordinal)).unwrap_err();
        assert!(error.contains("ordinal"), "{error}");
        assert!(error.contains("1 matching"), "{error}");
    }
}

#[test]
fn head_and_return_shifts_land_on_instruction_boundaries() {
    let classes = compile("head-return-shift");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let on_call = callback("onCall", "()V");
    let target = member("threeSites", "()V");
    let original = class
        .method("threeSites", "()V")
        .unwrap()
        .code
        .clone()
        .unwrap();
    let instructions = scan(&original);

    // `BY 0` is the plain HEAD weave; `BY 1` is the instruction after the
    // receiver load, and the callback still runs exactly once.
    let plain = weave_head(&class, &target, &on_call).unwrap();
    let zero = weave_head_shifted(&class, &target, &on_call, None, Shift::By(0)).unwrap();
    assert_eq!(zero, plain, "Shift.BY 0 is the HEAD insert");
    let head = weave_head_shifted(&class, &target, &on_call, None, Shift::By(1)).unwrap();
    let (code, _, _) = woven_code(&head, "threeSites", "()V");
    assert_insert_at(&original, &code, instructions[1].0);
    let lines = run(&classes, &head, "sites", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "");
    assert_eq!(value(&lines, "trace="), "isv");

    let error = weave_head_shifted(&class, &target, &on_call, None, Shift::By(-1)).unwrap_err();
    assert!(
        error.contains("runs past the method's first instruction"),
        "{error}"
    );

    // The same signed distance on a RETURN insert: -1 is the instruction that
    // pushed the first return value, so `pick(10)` still answers 1 with the
    // callback run first.
    let pick = member("pick", "(I)I");
    let pick_original = class.method("pick", "(I)I").unwrap().code.clone().unwrap();
    let pick_instructions = scan(&pick_original);
    let returns = pick_instructions
        .iter()
        .enumerate()
        .filter(|(_, (_, _, opcode))| matches!(opcode, 0xac..=0xb1))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(returns.len(), 3, "the fixture has three returns");
    let on_plain = callback("onPlain", "(I)V");
    let shifted = weave_returns_shifted(
        &class,
        &pick,
        &on_plain,
        false,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: Some(0),
            slice: None,
        },
        Shift::By(-1),
    )
    .unwrap();
    let (code, _, _) = woven_code(&shifted, "pick", "(I)I");
    let return_offset = pick_instructions[returns[0] - 1].0;
    let insert = assert_insert_before(&pick_original, &code, return_offset, 5);
    assert_eq!(insert[0], 0x15, "iload loads the method's argument");
    assert_eq!(insert[2], 0xb8, "then invokestatic onPlain");
    let lines = run(&classes, &shifted, "pick", Some("10"));
    assert_eq!(value(&lines, "result="), "1");
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "plain");

    let error = weave_returns_shifted(
        &class,
        &pick,
        &on_plain,
        false,
        &ReturnSelector {
            mode: ReturnMode::Return,
            ordinal: Some(-1),
            slice: None,
        },
        Shift::By(1),
    )
    .unwrap_err();
    assert!(
        error.contains("runs past the method's last instruction"),
        "{error}"
    );
}

#[test]
fn plan_driven_selectors_weave_and_run() {
    let classes = compile("plan-selectors");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let on_return = callback("onReturn", &format!("(I{CALLBACK_INFO_DESC})V"));
    let on_call = callback("onCall", "()V");
    let hit = method("demo/Target", "hit", "");

    // Two returns, and ordinal 1 names the second.
    let mut return_plan =
        MixinInjectionPlan::inject_at_return(member("twoReturns", "(I)I"), on_return);
    return_plan.ordinal = Some(1);

    // Two `hit` call sites, and ordinal -1 names the last.
    let mut invoke_plan = MixinInjectionPlan::inject_at_invoke(
        member("twoCalls", "()V"),
        on_call.clone(),
        hit.clone(),
    );
    invoke_plan.ordinal = Some(-1);

    // A slice counts only the sites between its boundaries; ordinal 0 is the
    // first `hit`.
    let mut slice_plan = MixinInjectionPlan::inject_at_invoke(
        member("twoCallsSlice", "()V"),
        on_call.clone(),
        hit.clone(),
    );
    slice_plan.ordinal = Some(0);
    slice_plan.slice = Some(SliceBounds {
        from: hit_on("(I)V"),
        to: hit_on("(Ljava/lang/String;)V"),
    });

    // The insert moves one instruction after the first call, between it and
    // `String.valueOf`.
    let mut shift_plan =
        MixinInjectionPlan::inject_at_invoke(member("twoCallsShift", "()V"), on_call, hit);
    shift_plan.ordinal = Some(0);
    shift_plan.shift = Some(InvokeShift {
        direction: Shift::After,
        by: 1,
    });

    let plans = [return_plan, invoke_plan, slice_plan, shift_plan];
    let woven = weave_class(&class, &plans).unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the plan-woven class round-trips"
    );

    // `twoReturns(1)` takes the first return, which ordinal 1 did not select;
    // `twoReturns(-1)` takes the second, which it did.
    let lines = run(&classes, &woven, "twoReturns", Some("1"));
    assert_eq!(value(&lines, "result="), "1");
    assert_eq!(value(&lines, "calls="), "0");
    let lines = run(&classes, &woven, "twoReturns", Some("-1"));
    assert_eq!(value(&lines, "result="), "2");
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "value="), "2");

    // The last call ran only after the first, so `seen` is `i`.
    let lines = run(&classes, &woven, "twoCalls", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "i");
    assert_eq!(value(&lines, "trace="), "is");

    // The slice holds the first `hit`, so the callback sees no trace yet.
    let lines = run(&classes, &woven, "twoCallsSlice", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "");
    assert_eq!(value(&lines, "trace="), "is");

    // The shifted insert runs after the first `hit`, before `String.valueOf`.
    let lines = run(&classes, &woven, "twoCallsShift", None);
    assert_eq!(value(&lines, "calls="), "1");
    assert_eq!(value(&lines, "seen="), "i");
    assert_eq!(value(&lines, "trace="), "is");
}

#[test]
fn discovery_parses_bounded_selectors_and_reports_the_rest() {
    let jar = compile_discovery("discover-selectors");
    let plans = jar.mixin_injection_plans();
    assert_eq!(plans.len(), 4, "unexpected plans: {plans:#?}");
    let plan = |name: &str| {
        plans
            .iter()
            .find(|plan| plan.callback.name == name)
            .unwrap_or_else(|| panic!("no plan for {name}: {plans:#?}"))
    };

    let returned = plan("onReturn");
    assert_eq!(returned.at, "RETURN");
    assert_eq!(returned.ordinal, Some(1));
    assert!(returned.slice.is_none());
    assert!(returned.shift.is_none());

    let invoked = plan("onInvoke");
    assert_eq!(invoked.at, "INVOKE");
    assert_eq!(invoked.ordinal, Some(-1));
    assert_eq!(
        invoked
            .invoke_target
            .as_ref()
            .map(|site| site.owner.as_str()),
        Some("demo/MixinTarget")
    );

    let sliced = plan("onSlice");
    assert_eq!(sliced.ordinal, None);
    assert_eq!(
        sliced.slice,
        Some(SliceBounds {
            from: SiteSelector::Invoke {
                owner: "demo/MixinTarget".into(),
                name: "hit".into(),
                descriptor: "(I)V".into(),
            },
            to: SiteSelector::Return,
        })
    );

    let shifted = plan("onShift");
    assert_eq!(
        shifted.shift,
        Some(InvokeShift {
            direction: Shift::After,
            by: 1,
        })
    );

    // A shift on a non-INVOKE anchor, an out-of-bound `by`, and a slice with
    // one bound are reported instead of guessed.
    let unresolved = jar.unresolved_invoke_plans();
    let names = unresolved
        .iter()
        .map(|plan| plan.callback.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        BTreeSet::from(["badBy", "badShift", "badSlice"]),
        "unresolved: {unresolved:#?}"
    );
}

/// `Code`, `max_stack` and `max_locals` of one woven method, plus the
/// round-trip check every woven class has to pass.
fn woven_code(woven: &[u8], name: &str, descriptor: &str) -> (Vec<u8>, u16, u16) {
    let class = ClassFile::parse(woven).unwrap();
    assert_eq!(class.write().unwrap(), woven, "woven class round-trips");
    let method = class.method(name, descriptor).unwrap();
    (
        method.code.clone().unwrap(),
        method.max_stack,
        method.max_locals,
    )
}

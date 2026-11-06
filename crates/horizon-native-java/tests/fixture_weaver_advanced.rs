//! Advanced weaver shapes: a conditional branch that grows past the 16-bit
//! range and a cancellable `void` `HEAD` injection.
//!
//! Both rewrites add a basic block the original class did not have (the split
//! conditional's fallthrough, the cancellable body), so these tests pin the
//! synthesised `StackMapTable` frame as well as the instruction sequence, and
//! then load the rewritten classes in a fresh `URLClassLoader`: only the real
//! verifier proves the frames are right.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, ReturnMode, callback_info_class,
    callback_info_returnable_class, weave_class, weave_class_with_extras, weave_head, weave_return,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const CALLBACK_INFO: &str = "org/spongepowered/asm/mixin/injection/callback/CallbackInfo";
const CALLBACK_INFO_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/callback/CallbackInfo;";
const CALLBACK_INFO_RETURNABLE: &str =
    "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable";
const CALLBACK_INFO_RETURNABLE_DESC: &str =
    "Lorg/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable;";

/// `javac` has to leave the conditional branch itself narrow: the body is a
/// statement short of the 16-bit limit, so only the weaver's insert pushes it
/// over.
const BIG_STATEMENTS: usize = 10_920;

const TARGET: &str = r#"package demo;
public class Target {
    public static int calls = 0;
    public static void run(int x) { calls += x; }
    public static int control(int x) { return x + 1; }
}"#;

const CALLBACK: &str = r#"package demo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
public final class Callback {
    public static void onRun(int x, CallbackInfo ci) {
        if (x < 0) { ci.cancel(); }
    }
    public static void onControl(int x) {}
    public static void onBig(int a, boolean flag, CallbackInfoReturnable cir) {
        if (a == 999) { cir.setReturnValue(Integer.valueOf(-1)); }
    }
}"#;

/// The driver: every class comes from the runner directory through a fresh
/// `URLClassLoader` whose parent is the platform loader.
const SMOKE: &str = r#"package driven;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Advanced {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> big = Class.forName("demo.Big", true, loader);
            Method method = big.getMethod("big", int.class, boolean.class);
            System.out.println("bigTrue=" + method.invoke(null, Integer.valueOf(7), Boolean.TRUE));
            System.out.println("bigFalse=" + method.invoke(null, Integer.valueOf(7), Boolean.FALSE));
            System.out.println("bigOverride=" + method.invoke(null, Integer.valueOf(999), Boolean.TRUE));

            Class<?> target = Class.forName("demo.Target", true, loader);
            Method run = target.getMethod("run", int.class);
            run.invoke(null, Integer.valueOf(-5));
            System.out.println("cancelled=" + target.getField("calls").getInt(null));
            run.invoke(null, Integer.valueOf(7));
            System.out.println("ran=" + target.getField("calls").getInt(null));
            Method control = target.getMethod("control", int.class);
            System.out.println("control=" + control.invoke(null, Integer.valueOf(3)));
        }
    }
}"#;

fn jdk_tool(name: &str) -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin").join(name))
        .unwrap_or_else(|| name.into())
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-advanced-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// `demo.Big`: a method whose `if (flag)` branch targets a `return` at the far
/// end of a ~32 KiB body.
fn big_source() -> String {
    let body = "        x += 1;\n".repeat(BIG_STATEMENTS);
    format!(
        "package demo;\npublic class Big {{\n    public static int big(int a, boolean flag) {{\n        int x = 0;\n        if (flag) {{\n{body}            return x;\n        }}\n        return 2;\n    }}\n}}\n"
    )
}

/// Write both synthetic callback classes into `classes`.
fn write_callback_classes(classes: &Path) {
    let info = classes.join(format!("{CALLBACK_INFO}.class"));
    fs::create_dir_all(info.parent().unwrap()).unwrap();
    fs::write(&info, callback_info_class().unwrap()).unwrap();
    let returnable = classes.join(format!("{CALLBACK_INFO_RETURNABLE}.class"));
    fs::create_dir_all(returnable.parent().unwrap()).unwrap();
    fs::write(&returnable, callback_info_returnable_class().unwrap()).unwrap();
}

fn compile(name: &str) -> PathBuf {
    let root = scratch(name);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    write_callback_classes(&classes);
    let sources = [
        ("demo/Target.java", TARGET.to_owned()),
        ("demo/Callback.java", CALLBACK.to_owned()),
        ("demo/Big.java", big_source()),
    ];
    let mut files = Vec::new();
    for (file, body) in sources {
        let path = src.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        files.push(path);
    }
    let output = Command::new(jdk_tool("javac"))
        .arg("--release")
        .arg("17")
        .arg("-nowarn")
        .arg("-cp")
        .arg(&classes)
        .arg("-d")
        .arg(&classes)
        .args(&files)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "javac failed for {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    classes
}

fn compile_driver(root: &Path) -> PathBuf {
    let driver = root.join("driver");
    let source = root.join("driver-src/driven/Advanced.java");
    fs::create_dir_all(&driver).unwrap();
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, SMOKE).unwrap();
    let output = Command::new(jdk_tool("javac"))
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&driver)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "javac failed for the driver: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    driver
}

fn read_class(classes: &Path, name: &str) -> Vec<u8> {
    fs::read(classes.join(format!("{name}.class"))).unwrap()
}

/// Write the runner directory: every compiled class, the two synthetic ones,
/// and the woven overrides.
fn runner(root: &Path, classes: &Path, overrides: &[(&str, Vec<u8>)]) -> PathBuf {
    let dir = root.join("runner");
    fs::create_dir_all(&dir).unwrap();
    for name in [
        "demo/Target",
        "demo/Callback",
        "demo/Big",
        CALLBACK_INFO,
        CALLBACK_INFO_RETURNABLE,
    ] {
        let bytes = match overrides
            .iter()
            .find(|(override_name, _)| *override_name == name)
        {
            Some((_, bytes)) => bytes.clone(),
            None => read_class(classes, name),
        };
        let path = dir.join(format!("{name}.class"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
    }
    dir
}

fn method(owner: &str, name: &str, descriptor: &str) -> MethodRef {
    MethodRef {
        owner: owner.into(),
        name: name.into(),
        descriptor: descriptor.into(),
    }
}

fn woven_code(woven: &[u8], name: &str, descriptor: &str) -> Vec<u8> {
    let class = ClassFile::parse(woven).unwrap();
    assert_eq!(class.write().unwrap(), woven, "woven class round-trips");
    class
        .method(name, descriptor)
        .unwrap()
        .code
        .clone()
        .unwrap()
}

fn count(code: &[u8], opcode: u8) -> usize {
    code.iter().filter(|byte| **byte == opcode).count()
}

/// The absolute bytecode offsets of one method's `StackMapTable` frames.
///
/// The public API exposes the code bytes but not the attribute tables, so this
/// is the smallest reader that reaches the frames: constant pool names, methods,
/// `Code` attributes, `StackMapTable` entries.
fn frame_offsets(class: &[u8], method_name: &str) -> Vec<usize> {
    let mut r = Reader {
        bytes: class,
        at: 0,
    };
    assert_eq!(r.u4(), 0xCAFE_BABE);
    r.skip(4);
    let count = r.u2() as usize;
    let mut utf8 = vec![None; count];
    let mut index = 1;
    while index < count {
        match r.u1() {
            1 => {
                let length = r.u2() as usize;
                let start = r.at;
                r.skip(length);
                utf8[index] =
                    Some(String::from_utf8_lossy(&class[start..start + length]).into_owned());
            }
            3 | 4 => r.skip(4),
            5 | 6 => {
                r.skip(8);
                index += 1;
            }
            7 | 8 | 16 | 19 | 20 => r.skip(2),
            9 | 10 | 11 | 12 | 17 | 18 => r.skip(4),
            15 => r.skip(3),
            other => panic!("unexpected constant tag {other}"),
        }
        index += 1;
    }
    r.skip(6); // access, this, super
    let interfaces = r.u2() as usize;
    r.skip(interfaces * 2);
    let fields = r.u2() as usize;
    for _ in 0..fields {
        r.skip(6);
        skip_attributes(&mut r);
    }
    let methods = r.u2() as usize;
    let mut offsets = Vec::new();
    for _ in 0..methods {
        r.skip(2);
        let name = utf8[r.u2() as usize].clone().unwrap_or_default();
        r.skip(2);
        let attributes = r.u2() as usize;
        for _ in 0..attributes {
            let attribute_name = utf8[r.u2() as usize].clone().unwrap_or_default();
            let length = r.u4() as usize;
            let body = r.bytes(length);
            if attribute_name == "Code" && name == method_name {
                offsets = code_frame_offsets(body, &utf8);
            }
        }
    }
    offsets
}

fn skip_attributes(r: &mut Reader<'_>) {
    let count = r.u2() as usize;
    for _ in 0..count {
        r.skip(2);
        let length = r.u4() as usize;
        r.skip(length);
    }
}

fn code_frame_offsets(body: &[u8], utf8: &[Option<String>]) -> Vec<usize> {
    let mut r = Reader { bytes: body, at: 0 };
    r.skip(4); // max_stack, max_locals
    let code_length = r.u4() as usize;
    r.skip(code_length);
    let handlers = r.u2() as usize;
    r.skip(handlers * 8);
    let attributes = r.u2() as usize;
    for _ in 0..attributes {
        let name = utf8[r.u2() as usize].clone().unwrap_or_default();
        let length = r.u4() as usize;
        let body = r.bytes(length);
        if name == "StackMapTable" {
            return parse_frames(body);
        }
    }
    Vec::new()
}

fn parse_frames(body: &[u8]) -> Vec<usize> {
    let mut r = Reader { bytes: body, at: 0 };
    let count = r.u2() as usize;
    let mut frames: Vec<usize> = Vec::with_capacity(count);
    for _ in 0..count {
        let frame_type = r.u1();
        let delta = match frame_type {
            0..=63 => frame_type as usize,
            64..=127 => {
                r.u1();
                frame_type as usize - 64
            }
            247 => {
                let delta = r.u2() as usize;
                r.u1();
                delta
            }
            248..=251 => r.u2() as usize,
            252..=254 => {
                let delta = r.u2() as usize;
                for _ in 0..(frame_type as usize - 251) {
                    skip_verification(&mut r);
                }
                delta
            }
            255 => {
                let delta = r.u2() as usize;
                let locals = r.u2() as usize;
                for _ in 0..locals {
                    skip_verification(&mut r);
                }
                let stack = r.u2() as usize;
                for _ in 0..stack {
                    skip_verification(&mut r);
                }
                delta
            }
            other => panic!("reserved frame type {other}"),
        };
        let offset = match frames.last() {
            Some(previous) => previous + delta + 1,
            None => delta,
        };
        frames.push(offset);
    }
    frames
}

fn skip_verification(r: &mut Reader<'_>) {
    match r.u1() {
        7 | 8 => r.skip(2),
        0..=6 => {}
        other => panic!("reserved verification type {other}"),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u1(&mut self) -> u8 {
        let value = self.bytes[self.at];
        self.at += 1;
        value
    }

    fn u2(&mut self) -> u16 {
        let value = u16::from_be_bytes([self.bytes[self.at], self.bytes[self.at + 1]]);
        self.at += 2;
        value
    }

    fn u4(&mut self) -> u32 {
        let bytes = &self.bytes[self.at..self.at + 4];
        self.at += 4;
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    fn skip(&mut self, count: usize) {
        self.at += count;
    }

    fn bytes(&mut self, count: usize) -> &'a [u8] {
        let start = self.at;
        self.at += count;
        &self.bytes[start..start + count]
    }
}

#[test]
fn the_synthetic_callback_info_parses_and_round_trips() {
    let bytes = callback_info_class().unwrap();
    let class = ClassFile::parse(&bytes).unwrap();
    assert_eq!(class.name, CALLBACK_INFO);
    assert_eq!(class.super_name, "java/lang/Object");
    assert_eq!(class.write().unwrap(), bytes);
    assert_eq!(
        class
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.descriptor.as_str()))
            .collect::<Vec<_>>(),
        vec![("cancelled", "Z")]
    );
    assert_eq!(
        class
            .methods
            .iter()
            .map(|method| (method.name.as_str(), method.descriptor.as_str()))
            .collect::<Vec<_>>(),
        vec![("<init>", "()V"), ("cancel", "()V"), ("isCancelled", "()Z"),]
    );
    for method in &class.methods {
        assert!(method.code.is_some(), "{} has no Code", method.name);
    }
}

#[test]
fn a_long_conditional_is_split_and_still_verifies() {
    let classes = compile("conditional");
    let class = ClassFile::parse(&read_class(&classes, "demo/Big")).unwrap();
    let target = method("demo/Big", "big", "(IZ)I");
    let callback = method(
        "demo/Callback",
        "onBig",
        &format!("(IZ{CALLBACK_INFO_RETURNABLE_DESC})V"),
    );
    let point = "demo.Big.big(IZ)I";
    let plan = MixinInjectionPlan::inject_at_return(target.clone(), callback.clone());
    let (woven, added) = weave_class_with_extras(&class, std::slice::from_ref(&plan)).unwrap();
    assert!(added.contains_key(CALLBACK_INFO_RETURNABLE));
    let code = woven_code(&woven, "big", "(IZ)I");

    // `ifeq` at 3 became `ifne +8; goto_w target`.
    assert_eq!(code[3], 0x9a, "the ifeq is inverted");
    assert_eq!(i16::from_be_bytes([code[4], code[5]]), 8);
    assert_eq!(code[6], 0xc8, "the far target is a goto_w");
    assert!(count(&code, 0xc8) >= 1);
    assert_eq!(
        code.iter().filter(|byte| **byte == 0xc8).count(),
        1,
        "{point}: only the split branch needs a wide jump"
    );

    // The fallthrough `iinc` was at offset 6; the 3-byte branch grew by 5, so
    // the synthesised frame sits at 11.  The far target keeps its own frame.
    let frames = frame_offsets(&woven, "big");
    assert!(
        frames.contains(&11),
        "the fallthrough frame is missing: {frames:?}"
    );
    assert!(
        frames.iter().any(|offset| *offset > 32_000),
        "the far target frame is missing: {frames:?}"
    );
    assert_eq!(
        frames.len(),
        class_frames(&read_class(&classes, "demo/Big")).len() + 1,
        "the split adds exactly one frame"
    );

    let root = classes.parent().unwrap().to_path_buf();
    let driver = compile_driver(&root);
    let runner = runner(&root, &classes, &[("demo/Big", woven.clone())]);
    let output = Command::new(jdk_tool("java"))
        .arg("-cp")
        .arg(&driver)
        .arg("driven.Advanced")
        .arg(&runner)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the JVM rejected the split conditional: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("bigTrue={BIG_STATEMENTS}")),
        "{stdout}"
    );
    assert!(stdout.contains("bigFalse=2"), "{stdout}");
    assert!(stdout.contains("bigOverride=-1"), "{stdout}");
}

fn class_frames(bytes: &[u8]) -> Vec<usize> {
    frame_offsets(bytes, "big")
}

#[test]
fn a_cancellable_void_head_skips_the_body_only_when_cancelled() {
    let classes = compile("cancel");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let run_target = method("demo/Target", "run", "(I)V");
    let run_callback = method(
        "demo/Callback",
        "onRun",
        &format!("(I{CALLBACK_INFO_DESC})V"),
    );
    let mut run = MixinInjectionPlan::inject_at_head(run_target, run_callback);
    run.cancellable = true;
    let control_target = method("demo/Target", "control", "(I)I");
    let control_callback = method("demo/Callback", "onControl", "(I)V");
    let control =
        MixinInjectionPlan::inject_at_head(control_target.clone(), control_callback.clone());

    // A non-cancellable HEAD plan stays byte-identical to `weave_head`.
    assert_eq!(
        weave_class(&class, std::slice::from_ref(&control)).unwrap(),
        weave_head(&class, &control_target, &control_callback).unwrap()
    );

    let (woven, added) = weave_class_with_extras(&class, &[control.clone(), run.clone()]).unwrap();
    assert!(added.contains_key(CALLBACK_INFO));
    let code = woven_code(&woven, "run", "(I)V");
    assert_eq!(code[0], 0xbb, "the insert starts with `new CallbackInfo`");
    assert!(count(&code, 0x99) >= 1, "the cancelled branch is an ifeq");
    assert!(count(&code, 0xb1) >= 1, "the cancelled path returns");
    // The insert is everything before the first original instruction; the
    // inserted `return` is its last byte.
    let body = code.iter().position(|byte| *byte == 0xb1).unwrap() + 1;
    assert_eq!(
        code[body], 0xb2,
        "the original `getstatic` follows the insert"
    );
    let frames = frame_offsets(&woven, "run");
    assert!(
        frames.contains(&body),
        "the cancellable body has no frame: {frames:?}"
    );

    // `control` is a plain HEAD insert: the woven bytes are unchanged by the
    // cancellable plan beside it.
    assert_eq!(
        weave_class(&class, &[control]).unwrap(),
        weave_head(&class, &control_target, &control_callback).unwrap()
    );

    let root = classes.parent().unwrap().to_path_buf();
    let driver = compile_driver(&root);
    let runner = runner(&root, &classes, &[("demo/Target", woven.clone())]);
    let output = Command::new(jdk_tool("java"))
        .arg("-cp")
        .arg(&driver)
        .arg("driven.Advanced")
        .arg(&runner)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the JVM rejected the cancellable HEAD: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cancelled=0"), "{stdout}");
    assert!(stdout.contains("ran=7"), "{stdout}");
    assert!(stdout.contains("control=4"), "{stdout}");
}

#[test]
fn weave_return_and_weave_head_agree_on_the_untouched_shapes() {
    let classes = compile("untouched");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let target = method("demo/Target", "run", "(I)V");
    let callback = method("demo/Callback", "onRun", "(I)V");
    let plain = weave_return(&class, &target, &callback, ReturnMode::Return).unwrap();
    let plain_class = ClassFile::parse(&plain).unwrap();
    assert_eq!(plain_class.write().unwrap(), plain);
    // A plain RETURN weave of a void target still has no branch and no
    // `CallbackInfo` construction.
    let code = woven_code(&plain, "run", "(I)V");
    assert_eq!(count(&code, 0xbb), 0);
    assert_eq!(count(&code, 0x99), 0);
}

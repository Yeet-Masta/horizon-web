//! Non-void `RETURN`/`TAIL` weaving: box, construct, call, read back.
//!
//! The woven method keeps its result in a fresh local, builds a synthetic
//! `CallbackInfoReturnable` around it, runs the callback and returns
//! `getReturnValue()`, so an override wins and the target's own value survives
//! when the callback sets none.  These tests pin that instruction sequence, the
//! class the weaver has to add beside the rewritten class, and - the only check
//! that covers the verifier - real JVM runs of rewritten classes inside a fresh
//! `URLClassLoader`.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, NativeJar, ReturnMode,
    callback_info_returnable_class, weave_class, weave_class_with_extras,
    weave_classes_with_extras, weave_return,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

const CALLBACK_INFO: &str = "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable";
const CALLBACK_INFO_DESC: &str =
    "Lorg/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable;";

const TARGET: &str = r#"package demo;
public class Target {
    public int value() { return 1; }
    public String name() { return "base"; }
    public long wide() { return 5L; }
    public static int fixed() { return 3; }
    public void plain() {}
    public int branchy(int a) { if (a > 0) { return 1; } return 2; }
}"#;

const CALLBACK: &str = r#"package demo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
public final class Callback {
    public static void overrideInt(Target self, CallbackInfoReturnable cir) {
        cir.setReturnValue(Integer.valueOf(7));
    }
    public static void readInt(Target self, CallbackInfoReturnable cir) {}
    public static void overrideText(Target self, CallbackInfoReturnable cir) {
        cir.setReturnValue("woven");
    }
    public static void readText(Target self, CallbackInfoReturnable cir) {}
    public static void overrideLong(Target self, CallbackInfoReturnable cir) {
        cir.setReturnValue(Long.valueOf(42L));
    }
    public static void overrideFixed(CallbackInfoReturnable cir) {
        cir.setReturnValue(Integer.valueOf(9));
    }
    public static void overrideBranchy(Target self, int a, CallbackInfoReturnable cir) {
        if (a > 0) { cir.setReturnValue(Integer.valueOf(11)); }
    }
    public static void overrideRaw(int a, CallbackInfoReturnable cir) {
        cir.setReturnValue(Integer.valueOf(a + 10));
    }
    public static void readRaw(int a, CallbackInfoReturnable cir) {}
}"#;

/// The driver the JVM runs: it loads the woven directory through a fresh
/// `URLClassLoader` whose parent is the platform loader, so no `demo` class can
/// come from the application classpath.
const SMOKE: &str = r#"package driven;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Paths;
public final class Smoke {
    public static void main(String[] args) throws Exception {
        URL[] urls = { Paths.get(args[0]).toUri().toURL() };
        try (URLClassLoader loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Class<?> target = Class.forName(args[1], true, loader);
            Method method;
            Object[] call;
            if (args.length > 3) {
                method = target.getMethod(args[2], int.class);
                call = new Object[] { Integer.valueOf(Integer.parseInt(args[3])) };
            } else {
                method = target.getMethod(args[2]);
                call = new Object[0];
            }
            Object instance = Modifier.isStatic(method.getModifiers())
                ? null
                : target.getDeclaredConstructor().newInstance();
            System.out.println("result=" + method.invoke(instance, call));
        }
    }
}"#;

/// The Mixin annotations `--weave` discovers a plan through.
const MIXIN_ANNOTATION: &str = r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface Mixin {
    String[] targets() default {};
    String[] value() default {};
}"#;

const AT_ANNOTATION: &str = r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.ANNOTATION_TYPE)
public @interface At {
    String value();
    String target() default "";
}"#;

const INJECT_ANNOTATION: &str = r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Inject {
    String[] method();
    At[] at();
    boolean cancellable() default false;
}"#;

const TARGET_MIXIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
@Mixin(targets = "Ldemo/Target;")
public abstract class TargetMixin {
    @Inject(method = "value()I", at = @At("RETURN"))
    private static void onValue(Target self, CallbackInfoReturnable cir) {
        cir.setReturnValue(Integer.valueOf(7));
    }
}"#;

fn jdk_tool(name: &str) -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin").join(name))
        .unwrap_or_else(|| name.into())
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-returns-value-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// Write the synthetic class into a directory javac and the JVM can read.
fn write_callback_info(classes: &Path) {
    let path = classes.join(format!("{CALLBACK_INFO}.class"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, callback_info_returnable_class().unwrap()).unwrap();
}

/// Compile `demo/Target`, `demo/Callback` and any extra sources against the
/// synthetic `CallbackInfoReturnable`.
fn compile(name: &str, extra: &[(&str, &str)]) -> PathBuf {
    let root = scratch(name);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    write_callback_info(&classes);
    let mut sources = vec![
        ("demo/Target.java", TARGET),
        ("demo/Callback.java", CALLBACK),
    ];
    sources.extend_from_slice(extra);
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

/// Compile the driver against no `demo` class at all.
fn compile_driver(root: &Path) -> PathBuf {
    let driver = root.join("driver");
    let source = root.join("driver-src/driven/Smoke.java");
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

/// Write the woven classes into `runner` and invoke one method through a fresh
/// `URLClassLoader`, returning the driver's `result=...` line.
fn run_in_jvm(
    driver: &Path,
    runner: &Path,
    woven: &[(&str, Vec<u8>)],
    class: &str,
    method: &str,
    argument: Option<&str>,
) -> String {
    for (name, bytes) in woven {
        let path = runner.join(format!("{name}.class"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
    }
    let mut command = Command::new(jdk_tool("java"));
    command
        .arg("-cp")
        .arg(driver)
        .arg("driven.Smoke")
        .arg(runner)
        .arg(class)
        .arg(method);
    if let Some(argument) = argument {
        command.arg(argument);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "the JVM rejected the woven class: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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

/// The `(Ldemo/Target;...CallbackInfoReturnable;)V` shape for one instance
/// callback.
fn instance_callback(name: &str) -> MethodRef {
    callback(name, &format!("(Ldemo/Target;{CALLBACK_INFO_DESC})V"))
}

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

fn count(code: &[u8], opcode: u8) -> usize {
    code.iter().filter(|byte| **byte == opcode).count()
}

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

/// `public static int demo/T.m(int)`, hand-built so one `ireturn` is a branch
/// target and therefore carries a `StackMapTable` frame:
///
/// ```text
/// 0: iload_0
/// 1: ifeq 8
/// 4: iconst_1
/// 5: goto 9
/// 8: iconst_2       frame: locals [Integer], stack []
/// 9: ireturn        frame: locals [Integer], stack [Integer]
/// ```
///
/// Pool: 1 `demo/T`, 2 Class 1, 3 `java/lang/Object`, 4 Class 3, 5 `m`,
/// 6 `(I)I`, 7 `Code`, 8 `StackMapTable`.
fn two_arm_class() -> Vec<u8> {
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
    u2(&mut out, 0x0021); // public
    u2(&mut out, 2); // this
    u2(&mut out, 4); // super
    u2(&mut out, 0); // interfaces
    u2(&mut out, 0); // fields
    u2(&mut out, 1); // methods
    u2(&mut out, 0x0009); // public static
    u2(&mut out, 5);
    u2(&mut out, 6);
    u2(&mut out, 1); // method attributes: Code
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

#[test]
fn the_synthetic_class_is_a_parsable_returnable() {
    let bytes = callback_info_returnable_class().unwrap();
    let class = ClassFile::parse(&bytes).unwrap();
    assert_eq!(class.name, CALLBACK_INFO);
    assert_eq!(class.super_name, "java/lang/Object");
    assert_eq!(class.write().unwrap(), bytes, "the class round-trips");
    let fields = class
        .fields
        .iter()
        .map(|field| (field.name.as_str(), field.descriptor.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        vec![("returnValue", "Ljava/lang/Object;"), ("cancelled", "Z")]
    );
    let methods = class
        .methods
        .iter()
        .map(|method| (method.name.as_str(), method.descriptor.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        vec![
            ("<init>", "()V"),
            ("<init>", "(Ljava/lang/Object;)V"),
            ("setReturnValue", "(Ljava/lang/Object;)V"),
            ("getReturnValue", "()Ljava/lang/Object;"),
            ("cancel", "()V"),
            ("isCancelled", "()Z"),
        ]
    );
    for method in &class.methods {
        assert!(method.code.is_some(), "{} has no Code", method.name);
        assert!(
            !method.is_static(),
            "{} must be an instance method",
            method.name
        );
    }
}

#[test]
fn return_boxes_the_result_and_reads_it_back() {
    let classes = compile("int", &[]);
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let original = class.method("value", "()I").unwrap().code.clone().unwrap();
    let before = class.method("value", "()I").unwrap().max_stack;
    let woven = weave_return(
        &class,
        &method("demo/Target", "value", "()I"),
        &instance_callback("overrideInt"),
        ReturnMode::Return,
    )
    .unwrap();
    let (code, max_stack, max_locals) = woven_code(&woven, "value", "()I");
    // `iconst_1` is the target's own push; the insert runs in front of `ireturn`.
    assert_eq!(code[0], 0x04, "{code:02x?}");
    let insert = &code[1..code.len() - 1];
    assert_eq!(
        (insert[0], insert[3], insert[4], insert[5]),
        (
            0xb8, // invokestatic Integer.valueOf (its operand follows)
            0x3a, // astore
            0x01, // the fresh local: past the only local the method declares
            0xbb, // new CallbackInfoReturnable
        ),
        "{code:02x?}"
    );
    assert_eq!(insert[8], 0x59, "dup");
    assert_eq!(&insert[9..11], &[0x19, 0x01], "aload <fresh local>");
    assert_eq!(insert[11], 0xb7, "invokespecial <init>(Object)");
    assert_eq!(
        &insert[14..16],
        &[0x3a, 0x01],
        "astore <fresh local>: the boxed value is dead once <init> ran"
    );
    assert_eq!(
        &insert[16..19],
        &[0x2a, 0x19, 0x01],
        "the receiver, then the object"
    );
    assert_eq!(insert[19], 0xb8, "invokestatic callback");
    assert_eq!(&insert[22..24], &[0x19, 0x01], "aload <fresh local>");
    assert_eq!(insert[24], 0xb6, "invokevirtual getReturnValue");
    assert_eq!(insert[27], 0xc0, "checkcast Integer");
    assert_eq!(insert[30], 0xb6, "invokevirtual intValue");
    assert_eq!(
        &code[code.len() - 1..],
        &[0xac],
        "the return is the target's own"
    );
    assert_eq!(code.len(), original.len() + 33);
    assert_eq!(count(insert, 0xb8), 2, "box once, call once");
    assert_eq!(count(insert, 0x59), 1, "one dup to construct the object");
    assert_eq!(count(insert, 0xc0), 1, "one checkcast before the unbox");
    assert_eq!(count(insert, 0xb6), 2, "getReturnValue and intValue");
    // Three construction slots plus the receiver, on top of the original peak.
    assert_eq!(max_stack, before + 1 + 3);
    assert_eq!(max_locals, 2, "the fresh local is past the declared ones");
    assert_eq!(original, vec![0x04, 0xac]);
}

#[test]
fn return_tags_a_reference_result_without_boxing() {
    let classes = compile("text", &[]);
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "name", "()Ljava/lang/String;"),
        &instance_callback("overrideText"),
        ReturnMode::Return,
    )
    .unwrap();
    let (code, _, max_locals) = woven_code(&woven, "name", "()Ljava/lang/String;");
    // A reference needs no `valueOf`: the result is already an `Object`.
    assert_eq!(count(&code, 0xb8), 1, "only the callback call: {code:02x?}");
    assert_eq!(count(&code, 0xc0), 1, "one checkcast, to the return type");
    assert_eq!(count(&code, 0xb6), 1, "getReturnValue only");
    assert_eq!(max_locals, 2);
}

#[test]
fn a_static_target_pushes_no_receiver() {
    let classes = compile("static", &[]);
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "fixed", "()I"),
        &callback("overrideFixed", &format!("({CALLBACK_INFO_DESC})V")),
        ReturnMode::Return,
    )
    .unwrap();
    let (code, _, max_locals) = woven_code(&woven, "fixed", "()I");
    assert_eq!(code[0], 0x06, "iconst_3 is the target's own push");
    let insert = &code[1..code.len() - 1];
    assert_eq!(
        (insert[0], insert[3], insert[4]),
        (0xb8, 0x3a, 0x00),
        "the wrapper's slot is the first free one: a static method has none"
    );
    assert_eq!(insert[5], 0xbb, "new CallbackInfoReturnable");
    assert_eq!(insert[8], 0x59, "dup");
    assert_eq!(
        &insert[14..16],
        &[0x3a, 0x00],
        "the object comes back to the fresh slot"
    );
    assert_eq!(
        &insert[16..18],
        &[0x19, 0x00],
        "no aload_0 for a static target: its first argument is the object"
    );
    assert_eq!(count(insert, 0x2a), 0);
    assert_eq!(max_locals, 1);
}

#[test]
fn tail_wraps_only_the_last_return_of_a_non_void_target() {
    let classes = compile("tail", &[]);
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let callback = callback(
        "overrideBranchy",
        &format!("(Ldemo/Target;I{CALLBACK_INFO_DESC})V"),
    );
    let every = weave_return(
        &class,
        &method("demo/Target", "branchy", "(I)I"),
        &callback,
        ReturnMode::Return,
    )
    .unwrap();
    let (code, _, _) = woven_code(&every, "branchy", "(I)I");
    assert_eq!(count(&code, 0xbb), 2, "one object per return: {code:02x?}");
    assert_eq!(count(&code, 0xac), 2, "both returns stay");

    let tail = weave_return(
        &class,
        &method("demo/Target", "branchy", "(I)I"),
        &callback,
        ReturnMode::Tail,
    )
    .unwrap();
    let (code, _, _) = woven_code(&tail, "branchy", "(I)I");
    assert_eq!(count(&code, 0xbb), 1, "TAIL wraps the last return only");
    assert_eq!(count(&code, 0xac), 2);
}

#[test]
fn weave_class_reports_the_synthetic_class_once() {
    let classes = compile("extras", &[]);
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let non_void = MixinInjectionPlan::inject_at_return(
        method("demo/Target", "value", "()I"),
        instance_callback("overrideInt"),
    );
    let (bytes, added) = weave_class_with_extras(&class, std::slice::from_ref(&non_void)).unwrap();
    assert_eq!(added.len(), 1);
    let expected = callback_info_returnable_class().unwrap();
    assert_eq!(
        added.get(CALLBACK_INFO).map(Vec::as_slice),
        Some(&expected[..])
    );
    assert_eq!(
        weave_class(&class, std::slice::from_ref(&non_void)).unwrap(),
        bytes
    );

    // A void target needs no object, so nothing is added.
    let void = MixinInjectionPlan::inject_at_return(
        method("demo/Target", "plain", "()V"),
        method("demo/Callback", "readInt", "(Ldemo/Target;)V"),
    );
    let (_, added) = weave_class_with_extras(&class, &[void]).unwrap();
    assert!(added.is_empty());

    // A jar that already declares the class keeps its own copy.
    let mut set = BTreeMap::new();
    set.insert("demo/Target".to_owned(), class.clone());
    set.insert(
        CALLBACK_INFO.to_owned(),
        ClassFile::parse(&callback_info_returnable_class().unwrap()).unwrap(),
    );
    let woven = weave_classes_with_extras(&set, &[non_void]).unwrap();
    assert!(woven.added.is_empty(), "{:?}", woven.added.keys());
    assert_eq!(woven.classes.len(), 2);
    assert_ne!(
        woven.classes["demo/Target"],
        set["demo/Target"].write().unwrap()
    );
}

#[test]
fn the_jvm_returns_the_override_or_the_original_value() {
    let classes = compile("jvm", &[]);
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let callback_class = read_class(&classes, "demo/Callback");
    let returnable = callback_info_returnable_class().unwrap();
    let root = scratch("jvm-run");
    let driver = compile_driver(&root);
    let weave = |name: &str, descriptor: &str, callback: &MethodRef| {
        weave_return(
            &class,
            &method("demo/Target", name, descriptor),
            callback,
            ReturnMode::Return,
        )
        .unwrap()
    };
    let run = |label: &str, woven: Vec<u8>, name: &str, argument: Option<&str>| {
        let runner = root.join(label);
        fs::create_dir_all(&runner).unwrap();
        run_in_jvm(
            &driver,
            &runner,
            &[
                ("demo/Target", woven),
                ("demo/Callback", callback_class.clone()),
                (CALLBACK_INFO, returnable.clone()),
            ],
            "demo.Target",
            name,
            argument,
        )
    };

    // `int value() { return 1; }`
    let override_int = weave("value", "()I", &instance_callback("overrideInt"));
    assert_eq!(run("int-override", override_int, "value", None), "result=7");
    let read_int = weave("value", "()I", &instance_callback("readInt"));
    assert_eq!(run("int-read", read_int, "value", None), "result=1");

    // `String name() { return "base"; }`
    let override_text = weave(
        "name",
        "()Ljava/lang/String;",
        &instance_callback("overrideText"),
    );
    assert_eq!(
        run("text-override", override_text, "name", None),
        "result=woven"
    );
    let read_text = weave(
        "name",
        "()Ljava/lang/String;",
        &instance_callback("readText"),
    );
    assert_eq!(run("text-read", read_text, "name", None), "result=base");

    // `long wide() { return 5L; }`
    let override_long = weave("wide", "()J", &instance_callback("overrideLong"));
    assert_eq!(
        run("long-override", override_long, "wide", None),
        "result=42"
    );

    // `static int fixed() { return 3; }`
    let override_fixed = weave(
        "fixed",
        "()I",
        &callback("overrideFixed", &format!("({CALLBACK_INFO_DESC})V")),
    );
    assert_eq!(run("static", override_fixed, "fixed", None), "result=9");

    // `int branchy(int a)`: the callback overrides one path only, so the other
    // return keeps the target's own value.
    let branchy = callback(
        "overrideBranchy",
        &format!("(Ldemo/Target;I{CALLBACK_INFO_DESC})V"),
    );
    let override_branchy = weave("branchy", "(I)I", &branchy);
    assert_eq!(
        run(
            "branchy-override",
            override_branchy.clone(),
            "branchy",
            Some("1")
        ),
        "result=11"
    );
    assert_eq!(
        run("branchy-read", override_branchy, "branchy", Some("-1")),
        "result=2"
    );
}

#[test]
fn the_jvm_verifies_a_return_that_is_a_branch_target() {
    let classes = compile("branch-target", &[]);
    let class = ClassFile::parse(&two_arm_class()).unwrap();
    let original = class.method("m", "(I)I").unwrap().code.clone().unwrap();
    let raw_callback = callback("overrideRaw", &format!("(I{CALLBACK_INFO_DESC})V"));
    let woven = weave_return(
        &class,
        &method("demo/T", "m", "(I)I"),
        &raw_callback,
        ReturnMode::Return,
    )
    .unwrap();
    let (code, _, max_locals) = woven_code(&woven, "m", "(I)I");
    // The `goto` that named the `ireturn` now names the insert, which sits
    // exactly where the return was: the insert is the one offset that keeps a
    // frame, because it changes the locals and the instruction behind it starts
    // no basic block.
    assert_eq!(
        &code[..9],
        &original[..9],
        "everything before the insert stays"
    );
    assert_eq!(code[5], 0xa7, "the goto is still there");
    let goto = i16::from_be_bytes([code[6], code[7]]);
    assert_eq!(
        goto, 4,
        "the goto enters the insert at the old return offset"
    );
    assert_eq!(code[8], 0x05, "iconst_2 keeps its offset");
    assert_eq!(code[9], 0xb8, "the insert starts where the return was");
    let insert = code.len() - original.len();
    assert_eq!(code[9 + insert], 0xac, "the return moved past the insert");
    assert_eq!(count(&code, 0xbb), 1);
    assert_eq!(max_locals, 2);

    let driver_root = scratch("branch-target-run");
    let driver = compile_driver(&driver_root);
    let callback_class = read_class(&classes, "demo/Callback");
    let returnable = callback_info_returnable_class().unwrap();
    for (label, argument, expected) in [
        ("override", "1", "result=11"),
        // `m(0)` takes the other arm; the callback runs there too.
        ("override-zero", "0", "result=10"),
    ] {
        let runner = driver_root.join(label);
        fs::create_dir_all(&runner).unwrap();
        let result = run_in_jvm(
            &driver,
            &runner,
            &[
                ("demo/T", woven.clone()),
                ("demo/Callback", callback_class.clone()),
                (CALLBACK_INFO, returnable.clone()),
            ],
            "demo.T",
            "m",
            Some(argument),
        );
        assert_eq!(result, expected, "{label}");
    }
    // No override: the same class through the reading callback keeps the arm's
    // own value.
    let read = weave_return(
        &class,
        &method("demo/T", "m", "(I)I"),
        &callback("readRaw", &format!("(I{CALLBACK_INFO_DESC})V")),
        ReturnMode::Return,
    )
    .unwrap();
    let runner = driver_root.join("read");
    fs::create_dir_all(&runner).unwrap();
    let result = run_in_jvm(
        &driver,
        &runner,
        &[
            ("demo/T", read),
            ("demo/Callback", callback_class),
            (CALLBACK_INFO, returnable),
        ],
        "demo.T",
        "m",
        Some("1"),
    );
    assert_eq!(result, "result=1");
}

#[test]
fn the_weave_cli_adds_the_synthetic_class_to_the_jar() {
    let classes = compile(
        "jar",
        &[
            ("org/spongepowered/asm/mixin/Mixin.java", MIXIN_ANNOTATION),
            (
                "org/spongepowered/asm/mixin/injection/At.java",
                AT_ANNOTATION,
            ),
            (
                "org/spongepowered/asm/mixin/injection/Inject.java",
                INJECT_ANNOTATION,
            ),
            ("demo/TargetMixin.java", TARGET_MIXIN),
        ],
    );
    let root = scratch("jar-run");
    let input = root.join("in.jar");
    let output = root.join("out.jar");
    let mut archive = zip::ZipWriter::new(fs::File::create(&input).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    for (name, bytes) in [
        ("demo/Target.class", read_class(&classes, "demo/Target")),
        (
            "demo/TargetMixin.class",
            read_class(&classes, "demo/TargetMixin"),
        ),
    ] {
        archive.start_file(name, options).unwrap();
        archive.write_all(&bytes).unwrap();
    }
    // The annotation classes a real jar carries, so the woven copy stays a
    // plausible mod.
    for entry in [
        "org/spongepowered/asm/mixin/Mixin.class",
        "org/spongepowered/asm/mixin/injection/At.class",
        "org/spongepowered/asm/mixin/injection/Inject.class",
    ] {
        let Ok(bytes) = fs::read(classes.join(entry)) else {
            continue;
        };
        archive.start_file(entry, options).unwrap();
        archive.write_all(&bytes).unwrap();
    }
    archive.start_file("fabric.mod.json", options).unwrap();
    archive
        .write_all(br#"{"id":"demo","version":"1.0.0"}"#)
        .unwrap();
    archive.finish().unwrap();

    let weave = Command::new(env!("CARGO_BIN_EXE_horizon-native-java"))
        .arg("--weave")
        .arg(&output)
        .arg("--json")
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        weave.status.success(),
        "weave failed: {}",
        String::from_utf8_lossy(&weave.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&weave.stdout).expect("the weave report is JSON");
    assert_eq!(report["status"], "ok");
    assert_eq!(report["woven_classes"], 1);
    assert_eq!(report["woven_plans"], 1);
    assert_eq!(report["skipped_plans"], 0);
    assert_eq!(report["added_classes"], 1);

    let mut archive = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
    let names = (0..archive.len())
        .map(|index| archive.by_index(index).unwrap().name().to_owned())
        .collect::<Vec<_>>();
    assert!(
        names.contains(&format!("{CALLBACK_INFO}.class")),
        "the woven jar has no synthetic class: {names:?}"
    );
    assert!(names.contains(&"demo/Target.class".to_owned()));
    assert!(names.contains(&"fabric.mod.json".to_owned()));
    let added = {
        let mut entry = archive.by_name(&format!("{CALLBACK_INFO}.class")).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        bytes
    };
    assert_eq!(
        ClassFile::parse(&added).unwrap().name,
        CALLBACK_INFO,
        "the added entry parses as the class it is named after"
    );
    // The native loader parses every class entry of the woven jar, the added one
    // included.
    let jar = NativeJar::from_path(&output).unwrap();
    assert_eq!(jar.mixin_injection_plans().len(), 1);
    let _ = fs::remove_dir_all(root);
}

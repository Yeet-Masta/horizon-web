//! Structural tests for the bounded class writer and the HEAD weaver.
//!
//! The fixtures come from `javac` so the writer is exercised against real
//! constant pools, attribute tables and debug attributes.  The rewritten class
//! is parsed back with [`ClassFile::parse`] instead of loaded in a JVM: the
//! crate's contract is structural, and the assertions below pin the inserted
//! instruction sequence, the adjusted `max_stack`, and the untouched bytes.

use horizon_native_java::{ClassFile, MethodRef, MixinInjectionPlan, weave_head, weave_plans};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = r#"package demo;
public class Target {
    public int add(int a, int b) { return a + b; }
    public static int addStatic(int a, int b) { return a + b; }
    public void noArgs() {}
    public int branchy(int a) { if (a > 0) { return a; } return -a; }
    public int guarded(int a) {
        try { return a; } catch (RuntimeException e) { return 0; }
    }
}"#;

const CALLBACK: &str = r#"package demo;
public final class Callback {
    public static void onAdd(Target self, int a, int b) {}
    public static void onAddStatic(int a, int b) {}
    public static void onNoArgs(Target self) {}
    public static void onAddRaw(int a, int b) {}
}"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn javap() -> Option<PathBuf> {
    let path = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javap"))
        .unwrap_or_else(|| "javap".into());
    Command::new(&path).arg("-version").output().ok()?;
    Some(path)
}

/// Compile `Target` and `Callback` into a fresh directory.
fn compile(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(&classes).unwrap();
    fs::write(src.join("demo/Target.java"), TARGET).unwrap();
    fs::write(src.join("demo/Callback.java"), CALLBACK).unwrap();
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .arg(src.join("demo/Target.java"))
        .arg(src.join("demo/Callback.java"))
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

fn max_stack(class: &ClassFile, name: &str, descriptor: &str) -> u16 {
    class.method(name, descriptor).unwrap().max_stack
}

#[test]
fn write_round_trips_javac_classes() {
    let classes = compile("round-trip");
    for name in ["demo/Target", "demo/Callback"] {
        let bytes = read_class(&classes, name);
        let class = ClassFile::parse(&bytes).unwrap();
        assert_eq!(class.write().unwrap(), bytes, "{name} round-trip");
    }
}

#[test]
fn head_weaves_a_static_method() {
    let classes = compile("static");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let original = class
        .method("addStatic", "(II)I")
        .unwrap()
        .code
        .clone()
        .unwrap();
    let woven = weave_head(
        &class,
        &method("demo/Target", "addStatic", "(II)I"),
        &method("demo/Callback", "onAddStatic", "(II)V"),
    )
    .unwrap();

    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    assert_eq!(reparsed.methods.len(), class.methods.len());
    let target = reparsed.method("addStatic", "(II)I").unwrap();
    let code = target.code.as_ref().unwrap();
    assert_eq!(&code[..4], &[0x15, 0x00, 0x15, 0x01]);
    assert_eq!(code[4], 0xb8); // invokestatic
    assert_eq!(&code[7..], original.as_slice());
    assert_eq!(
        target.max_stack,
        max_stack(&class, "addStatic", "(II)I") + 2 + 1
    );
    for name in ["add", "noArgs"] {
        let before = class.methods.iter().find(|m| m.name == name).unwrap();
        let after = reparsed.methods.iter().find(|m| m.name == name).unwrap();
        assert_eq!(after.code, before.code, "{name} is untouched");
    }
}

#[test]
fn head_weaves_an_instance_method_with_the_receiver() {
    let classes = compile("instance");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let original = class.method("add", "(II)I").unwrap().code.clone().unwrap();
    let woven = weave_head(
        &class,
        &method("demo/Target", "add", "(II)I"),
        &method("demo/Callback", "onAdd", "(Ldemo/Target;II)V"),
    )
    .unwrap();

    let reparsed = ClassFile::parse(&woven).unwrap();
    let target = reparsed.method("add", "(II)I").unwrap();
    let code = target.code.as_ref().unwrap();
    assert_eq!(&code[..5], &[0x2a, 0x15, 0x01, 0x15, 0x02]);
    assert_eq!(&code[8..], original.as_slice());
    assert_eq!(target.max_stack, max_stack(&class, "add", "(II)I") + 3 + 1);

    // Without the receiver parameter, no `aload_0` is inserted.
    let plain = weave_head(
        &class,
        &method("demo/Target", "add", "(II)I"),
        &method("demo/Callback", "onAddStatic", "(II)V"),
    )
    .unwrap();
    let plain = ClassFile::parse(&plain).unwrap();
    let code = plain.method("add", "(II)I").unwrap().code.as_ref().unwrap();
    assert_eq!(&code[..4], &[0x15, 0x01, 0x15, 0x02]);
    assert_eq!(code[4], 0xb8);
    assert_eq!(&code[7..], original.as_slice());
}

#[test]
fn head_weaves_a_no_argument_void_method() {
    let classes = compile("no-args");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let original = class.method("noArgs", "()V").unwrap().code.clone().unwrap();
    let woven = weave_head(
        &class,
        &method("demo/Target", "noArgs", "()V"),
        &method("demo/Callback", "onNoArgs", "(Ldemo/Target;)V"),
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    let code = reparsed
        .method("noArgs", "()V")
        .unwrap()
        .code
        .as_ref()
        .unwrap();
    assert_eq!(&code[..1], &[0x2a]);
    assert_eq!(&code[4..], original.as_slice());
}

#[test]
fn weave_plans_applies_only_matching_head_plans() {
    let classes = compile("plans");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let target = method("demo/Target", "addStatic", "(II)I");
    let callback = method("demo/Callback", "onAddStatic", "(II)V");
    let plans = [
        MixinInjectionPlan::inject_at_return(
            method("demo/Target", "addStatic", "(II)I"),
            method("demo/Callback", "onAddStatic", "(II)V"),
        ),
        MixinInjectionPlan::inject_at_head(
            method("demo/Other", "addStatic", "(II)I"),
            method("demo/Callback", "onAddStatic", "(II)V"),
        ),
        MixinInjectionPlan::inject_at_head(target.clone(), callback.clone()),
    ];
    let planned = weave_plans(&class, &plans).unwrap();
    let single = weave_head(&class, &target, &callback).unwrap();
    assert_eq!(planned, single);
}

#[test]
fn weave_plans_stacks_each_matching_head_plan() {
    let classes = compile("stacked-plans");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let plans = [
        MixinInjectionPlan::inject_at_head(
            method("demo/Target", "addStatic", "(II)I"),
            method("demo/Callback", "onAddStatic", "(II)V"),
        ),
        MixinInjectionPlan::inject_at_head(
            method("demo/Target", "addStatic", "(II)I"),
            method("demo/Callback", "onAddRaw", "(II)V"),
        ),
    ];
    let woven = weave_plans(&class, &plans).unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    let code = reparsed
        .method("addStatic", "(II)I")
        .unwrap()
        .code
        .as_ref()
        .unwrap();
    // The last plan is inserted last, so it runs first: two invokestatic
    // callbacks before the original body.
    assert_eq!(code.iter().filter(|opcode| **opcode == 0xb8).count(), 2);
}

#[test]
fn rejection_names_the_reason() {
    let classes = compile("rejections");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();

    // `branchy` and `guarded` are now supported: their branches, exception
    // table and `StackMapTable` are relocated (see `fixture_weaver_frames.rs`).
    let static_receiver = weave_head(
        &class,
        &method("demo/Target", "addStatic", "(II)I"),
        &method("demo/Callback", "onAdd", "(Ldemo/Target;II)V"),
    )
    .unwrap_err();
    assert!(
        static_receiver.contains("static target expects"),
        "{static_receiver}"
    );

    let descriptor = weave_head(
        &class,
        &method("demo/Target", "add", "(II)I"),
        &method("demo/Callback", "onAdd", "(Ldemo/Target;I)V"),
    )
    .unwrap_err();
    assert!(descriptor.contains("does not match"), "{descriptor}");
}

#[test]
fn javap_sees_the_inserted_call() {
    let Some(javap) = javap() else {
        return;
    };
    let classes = compile("javap");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_head(
        &class,
        &method("demo/Target", "addStatic", "(II)I"),
        &method("demo/Callback", "onAddStatic", "(II)V"),
    )
    .unwrap();
    let output = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-javap-{}/demo",
        std::process::id()
    ));
    fs::create_dir_all(&output).unwrap();
    let path = output.join("Target.class");
    fs::write(&path, woven).unwrap();
    let output = Command::new(javap)
        .arg("-c")
        .arg("-p")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "javap rejected the woven class");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("onAddStatic"), "{text}");
    assert!(text.contains("invokestatic"), "{text}");
}

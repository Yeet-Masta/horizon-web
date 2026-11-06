//! Structural tests for `RETURN`/`TAIL` weaving and whole-class weaving.
//!
//! The fixtures come from `javac` so the writer is exercised against real
//! constant pools, attribute tables and debug attributes.  `javac` cannot emit
//! a straight-line method with two return instructions (a second return needs a
//! branch), so that one fixture is assembled directly; every other class here
//! is compiled.  The rewritten class is parsed back with [`ClassFile::parse`]
//! and, when a JDK is present, disassembled with `javap -c` instead of being
//! loaded in a JVM: the crate's contract is structural, and the assertions pin
//! the inserted instruction sequence, the adjusted `max_stack`, and the
//! untouched bytes.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, ReturnMode, weave_class, weave_classes, weave_return,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = r#"package demo;
public class Target {
    public void one() {}
    public void withArgs(int a, String b) { System.out.print(a); System.out.print(b); }
    public int value() { return 1; }
    public void branchy(boolean b) { if (b) { return; } }
}"#;

const OTHER: &str = r#"package demo;
public class Other {
    public void other() {}
}"#;

const CALLBACK: &str = r#"package demo;
public final class Callback {
    public static void onOne(Target self) {}
    public static void onWithArgs(Target self, int a, String b) {}
    public static void onWithArgsRaw(int a, String b) {}
    public static void onStatic() {}
    public static void onValue(Target self) {}
    public static void onBranchy(Target self, boolean b) {}
}"#;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

/// Compile `Target`, `Other` and `Callback` into a fresh directory.
fn compile(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-returns-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src/demo");
    let classes = root.join("classes");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&classes).unwrap();
    for (file, body) in [
        ("Target.java", TARGET),
        ("Other.java", OTHER),
        ("Callback.java", CALLBACK),
    ] {
        fs::write(src.join(file), body).unwrap();
    }
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .arg(src.join("Target.java"))
        .arg(src.join("Other.java"))
        .arg(src.join("Callback.java"))
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

/// The `javap -c` block for one method, from its header to the next blank line.
fn javap_method_section<'a>(text: &'a str, header: &str) -> &'a str {
    let start = text
        .find(header)
        .unwrap_or_else(|| panic!("javap output has no {header:?}:\n{text}"));
    let rest = &text[start + header.len()..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    &rest[..end]
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

/// A straight-line `public static void demo/T.m()` whose code is `return;
/// return;`.  The second return is unreachable, which is exactly what `javac`
/// refuses to emit while still being a valid straight-line method.
///
/// Pool: 1 `demo/T`, 2 Class 1, 3 `java/lang/Object`, 4 Class 3, 5 `m`,
/// 6 `()V`, 7 `Code`.
fn two_return_class() -> Vec<u8> {
    let mut out = Vec::new();
    u4(&mut out, 0xCAFE_BABE);
    u2(&mut out, 0);
    u2(&mut out, 52);
    u2(&mut out, 8);
    utf8(&mut out, "demo/T");
    out.extend_from_slice(&[7, 0, 1]);
    utf8(&mut out, "java/lang/Object");
    out.extend_from_slice(&[7, 0, 3]);
    utf8(&mut out, "m");
    utf8(&mut out, "()V");
    utf8(&mut out, "Code");
    u2(&mut out, 0x0021); // class access
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
    let mut body = Vec::new();
    u2(&mut body, 0); // max_stack
    u2(&mut body, 0); // max_locals
    u4(&mut body, 2);
    body.extend_from_slice(&[0xb1, 0xb1]); // return; return;
    u2(&mut body, 0); // exception table
    u2(&mut body, 0); // Code attributes
    u4(&mut out, body.len() as u32);
    out.extend_from_slice(&body);
    u2(&mut out, 0); // class attributes
    out
}

/// `javap -c -p` output, or `None` when no JDK is installed.
fn javap_report(path: &Path) -> Option<String> {
    let javap = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javap"))
        .unwrap_or_else(|| "javap".into());
    Command::new(&javap).arg("-version").output().ok()?;
    let output = Command::new(&javap)
        .arg("-c")
        .arg("-p")
        .arg(path)
        .output()
        .ok()?;
    assert!(output.status.success(), "javap rejected {}", path.display());
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn return_weaves_before_a_javac_void_return() {
    let classes = compile("one-return");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let original = class.method("one", "()V").unwrap().code.clone().unwrap();
    let woven = weave_return(
        &class,
        &method("demo/Target", "one", "()V"),
        &method("demo/Callback", "onOne", "(Ldemo/Target;)V"),
        ReturnMode::Return,
    )
    .unwrap();

    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let target = reparsed.method("one", "()V").unwrap();
    let code = target.code.as_ref().unwrap();
    assert_eq!(code.len(), original.len() + 4);
    assert_eq!(&code[..2], &[0x2a, 0xb8]); // aload_0, invokestatic
    assert_eq!(&code[4..], original.as_slice());
    assert_eq!(target.max_stack, max_stack(&class, "one", "()V") + 1);
}

#[test]
fn return_loads_arguments_and_skips_the_receiver_when_not_declared() {
    let classes = compile("return-args");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let original = class
        .method("withArgs", "(ILjava/lang/String;)V")
        .unwrap()
        .code
        .clone()
        .unwrap();

    let with_receiver = weave_return(
        &class,
        &method("demo/Target", "withArgs", "(ILjava/lang/String;)V"),
        &method(
            "demo/Callback",
            "onWithArgs",
            "(Ldemo/Target;ILjava/lang/String;)V",
        ),
        ReturnMode::Return,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&with_receiver).unwrap();
    let target = reparsed
        .method("withArgs", "(ILjava/lang/String;)V")
        .unwrap();
    let code = target.code.as_ref().unwrap();
    let ret = original.len() - 1; // the original `return` instruction
    assert_eq!(&code[..ret], &original[..ret]);
    assert_eq!(
        &code[ret..ret + 5],
        &[0x2a, 0x15, 0x01, 0x19, 0x02] // aload_0, iload_1, aload_2
    );
    assert_eq!(&code[ret + 8..], &original[ret..]);
    assert_eq!(
        target.max_stack,
        max_stack(&class, "withArgs", "(ILjava/lang/String;)V") + 3
    );

    let raw = weave_return(
        &class,
        &method("demo/Target", "withArgs", "(ILjava/lang/String;)V"),
        &method("demo/Callback", "onWithArgsRaw", "(ILjava/lang/String;)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&raw).unwrap();
    let target = reparsed
        .method("withArgs", "(ILjava/lang/String;)V")
        .unwrap();
    let code = target.code.as_ref().unwrap();
    assert_eq!(&code[..ret], &original[..ret]);
    assert_eq!(&code[ret..ret + 4], &[0x15, 0x01, 0x19, 0x02]);
    assert_eq!(&code[ret + 7..], &original[ret..]);
    assert_eq!(
        target.max_stack,
        max_stack(&class, "withArgs", "(ILjava/lang/String;)V") + 2
    );
}

#[test]
fn return_weaves_every_return_and_tail_weaves_the_last_one() {
    let class = ClassFile::parse(&two_return_class()).unwrap();
    let target = method("demo/T", "m", "()V");
    let callback = method("demo/Callback", "onStatic", "()V");

    let every = weave_return(&class, &target, &callback, ReturnMode::Return).unwrap();
    let reparsed = ClassFile::parse(&every).unwrap();
    assert_eq!(reparsed.write().unwrap(), every);
    let code = reparsed.method("m", "()V").unwrap().code.as_ref().unwrap();
    assert_eq!(code.len(), 2 + 2 * 3); // invokestatic is three bytes
    assert_eq!(code.iter().filter(|opcode| **opcode == 0xb8).count(), 2);
    assert_eq!(code[0], 0xb8);
    assert_eq!(code[3], 0xb1);
    assert_eq!(code[4], 0xb8);
    assert_eq!(code[7], 0xb1);
    // A no-argument void callback pushes nothing before `invokestatic`.
    assert_eq!(reparsed.method("m", "()V").unwrap().max_stack, 0);

    let tail = weave_return(&class, &target, &callback, ReturnMode::Tail).unwrap();
    let reparsed = ClassFile::parse(&tail).unwrap();
    let code = reparsed.method("m", "()V").unwrap().code.as_ref().unwrap();
    assert_eq!(code.len(), 1 + 3 + 1);
    assert_eq!(code[0], 0xb1);
    assert_eq!(code[1], 0xb8);
    assert_eq!(code[4], 0xb1);

    // The TAIL plan through `weave_class` is the same edit.
    let plan = MixinInjectionPlan::inject_at_tail(target, callback);
    assert_eq!(weave_class(&class, &[plan]).unwrap(), tail);
}

#[test]
fn weave_class_applies_head_then_return_plans() {
    let classes = compile("class-order");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let target = method("demo/Target", "one", "()V");
    let callback = method("demo/Callback", "onOne", "(Ldemo/Target;)V");
    // Declared RETURN first: `weave_class` still applies HEAD first.
    let plans = [
        MixinInjectionPlan::inject_at_return(target.clone(), callback.clone()),
        MixinInjectionPlan::inject_at_head(target, callback),
    ];
    let woven = weave_class(&class, &plans).unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    let target = reparsed.method("one", "()V").unwrap();
    let code = target.code.as_ref().unwrap();
    assert_eq!(code.len(), 9);
    assert_eq!(&code[..4], &code[4..8], "HEAD call precedes the RETURN one");
    assert_eq!(code[8], 0xb1);
    assert_eq!(target.max_stack, 3);
    // A plan for another class is skipped by `weave_class`.
    let other = MixinInjectionPlan::inject_at_return(
        method("demo/Other", "other", "()V"),
        method("demo/Callback", "onStatic", "()V"),
    );
    assert_eq!(
        weave_class(&class, &[other]).unwrap(),
        class.write().unwrap()
    );
}

#[test]
fn weave_classes_weaves_affected_classes_and_reencodes_the_rest() {
    let classes = compile("class-set");
    let mut parsed = BTreeMap::new();
    let target_bytes = read_class(&classes, "demo/Target");
    let other_bytes = read_class(&classes, "demo/Other");
    parsed.insert(
        "demo/Target".to_owned(),
        ClassFile::parse(&target_bytes).unwrap(),
    );
    parsed.insert(
        "demo/Other".to_owned(),
        ClassFile::parse(&other_bytes).unwrap(),
    );
    let plans = [MixinInjectionPlan::inject_at_return(
        method("demo/Target", "one", "()V"),
        method("demo/Callback", "onOne", "(Ldemo/Target;)V"),
    )];
    let woven = weave_classes(&parsed, &plans).unwrap();
    assert_eq!(woven.len(), 2);
    assert_eq!(woven["demo/Other"], parsed["demo/Other"].write().unwrap());
    assert_eq!(
        woven["demo/Target"],
        weave_class(&parsed["demo/Target"], &plans).unwrap()
    );
    assert_ne!(woven["demo/Target"], target_bytes);

    let missing = [MixinInjectionPlan::inject_at_return(
        method("demo/Missing", "m", "()V"),
        method("demo/Callback", "onStatic", "()V"),
    )];
    let error = weave_classes(&parsed, &missing).unwrap_err();
    assert!(error.contains("not in the class set"), "{error}");
}

#[test]
fn return_rejects_a_non_void_target() {
    let classes = compile("non-void");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let error = weave_return(
        &class,
        &method("demo/Target", "value", "()I"),
        &method("demo/Callback", "onValue", "(Ldemo/Target;)V"),
        ReturnMode::Return,
    )
    .unwrap_err();
    assert!(error.contains("non-void"), "{error}");
    assert!(error.contains("CallbackInfoReturnable"), "{error}");
}

#[test]
fn return_weaves_a_javac_method_with_two_returns() {
    let classes = compile("branchy");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    // `javac` gives the second return a branch and a `StackMapTable`; both are
    // relocated now, so the plan lands on every return.
    let woven = weave_return(
        &class,
        &method("demo/Target", "branchy", "(Z)V"),
        &method("demo/Callback", "onBranchy", "(Ldemo/Target;Z)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(reparsed.write().unwrap(), woven, "woven class round-trips");
    let code = reparsed
        .method("branchy", "(Z)V")
        .unwrap()
        .code
        .as_ref()
        .unwrap();
    assert_eq!(code.iter().filter(|opcode| **opcode == 0xb8).count(), 2);
    assert_eq!(code.iter().filter(|opcode| **opcode == 0xb1).count(), 2);
}

#[test]
fn weave_class_rejects_cancellable_and_invoke_plans() {
    let classes = compile("reject-plans");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let target = method("demo/Target", "one", "()V");
    let callback = method("demo/Callback", "onOne", "(Ldemo/Target;)V");

    let mut cancellable = MixinInjectionPlan::inject_at_return(target.clone(), callback.clone());
    cancellable.cancellable = true;
    let error = weave_class(&class, &[cancellable]).unwrap_err();
    assert!(error.contains("cancellable"), "{error}");
    assert!(error.contains("CallbackInfo"), "{error}");

    let invoke = MixinInjectionPlan::inject_at_invoke(target, callback.clone(), callback);
    let error = weave_class(&class, &[invoke]).unwrap_err();
    assert!(error.contains("INVOKE"), "{error}");
    assert!(error.contains("interpreter"), "{error}");
}

#[test]
fn javap_sees_the_call_before_each_return() {
    let classes = compile("javap-returns");
    let class = ClassFile::parse(&read_class(&classes, "demo/Target")).unwrap();
    let one = weave_return(
        &class,
        &method("demo/Target", "one", "()V"),
        &method("demo/Callback", "onOne", "(Ldemo/Target;)V"),
        ReturnMode::Return,
    )
    .unwrap();
    let synthetic = ClassFile::parse(&two_return_class()).unwrap();
    let every = weave_return(
        &synthetic,
        &method("demo/T", "m", "()V"),
        &method("demo/Callback", "onStatic", "()V"),
        ReturnMode::Return,
    )
    .unwrap();

    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-returns-javap-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("demo")).unwrap();
    let target_path = root.join("demo/Target.class");
    let synthetic_path = root.join("demo/T.class");
    fs::write(&target_path, one).unwrap();
    fs::write(&synthetic_path, every).unwrap();

    let Some(text) = javap_report(&target_path) else {
        return;
    };
    // Scope the assertions to the woven method: the constructor also returns.
    let section = javap_method_section(&text, "public void one();");
    assert_eq!(
        section
            .lines()
            .filter(|line| line.contains("invokestatic") && line.contains("onOne"))
            .count(),
        1,
        "{section}"
    );
    let call = section
        .lines()
        .position(|line| line.contains("invokestatic"));
    let ret = section
        .lines()
        .position(|line| line.trim_end().ends_with(": return"));
    assert!(
        matches!((call, ret), (Some(call), Some(ret)) if call < ret),
        "{section}"
    );

    let text = javap_report(&synthetic_path).unwrap();
    let section = javap_method_section(&text, "public static void m();");
    assert_eq!(
        section
            .lines()
            .filter(|line| line.contains("invokestatic"))
            .count(),
        2,
        "{section}"
    );
    assert_eq!(
        section
            .lines()
            .filter(|line| line.trim_end().ends_with(": return"))
            .count(),
        2,
        "{section}"
    );
}

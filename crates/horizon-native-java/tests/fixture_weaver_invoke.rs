//! Structural and runtime tests for the bounded `@At("INVOKE")` weaver.
//!
//! The target has one `invokestatic` site, one `invokevirtual` site and one
//! cancellable `void` site.  Each woven class is loaded in a fresh JVM and run,
//! so the assertions cover the verifier as well as the callback behaviour: the
//! callback sees the call, and `ci.cancel()` suppresses only the selected call
//! while the woven method keeps running.

use horizon_native_java::{
    ClassFile, MethodRef, MixinInjectionPlan, weave_invoke, weave_invoke_plan,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = r#"package demo;
public class Target {
    public static int marked;
    public static int poked;
    public static String after = "";
    public final Sink sink = new SinkImpl();
    public static void mark(int value) { marked += value; }
    public static int value(int x) { return x * 2; }
    public void poke(int x) { poked += x; }
    public static void staticSite(int x) { mark(x); }
    public static void noArgSite() { mark(1); }
    public void instanceSite(int x) { poke(x); }
    public void interfaceSite(int x) { sink.accept(x); }
    public void interfaceCancelSite(int x) { sink.accept(x); after = "iface"; }
    public static int valueSite(int x) { return value(x); }
    public static void cancelSite(int x) { mark(x); after = "done"; }
    public static void twice(int x) { mark(x); mark(x); }
    public static void plain(int x) { mark(x); }
    public void make() { new Target(); }
    public static final class SinkImpl implements Sink {
        public void accept(int x) { poked += x; }
    }
}"#;

const SINK: &str = r#"package demo;
public interface Sink {
    void accept(int x);
}"#;

const CALLBACK: &str = r#"package demo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
public final class Callback {
    public static int calls;
    public static int last;
    public static void onStatic(int x) { calls++; last = x; }
    public static void onNoArg() { calls++; last = 42; }
    public static void onInstance(Target self, int x) { calls++; last = x; }
    public static void onCancel(int x, CallbackInfo ci) { calls++; last = x; ci.cancel(); }
    public static void onMake() { calls++; }
    public static void wrong(String x) { calls++; }
}"#;

const CALLBACK_INFO: &str = r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfo {
    private boolean cancelled;
    public void cancel() { cancelled = true; }
    public boolean isCancelled() { return cancelled; }
}"#;

const RUNNER: &str = r#"import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Path;
import java.nio.file.Paths;

class Runner {
    public static void main(String[] args) throws Exception {
        Path dir = Paths.get(args[0]);
        String which = args[1];
        try (URLClassLoader loader =
                new URLClassLoader(new URL[] { dir.toUri().toURL() }, Runner.class.getClassLoader())) {
            Class<?> target = Class.forName("demo.Target", true, loader);
            Object instance = target.getConstructor().newInstance();
            if (which.equals("static")) {
                target.getMethod("staticSite", int.class).invoke(null, 5);
            } else if (which.equals("noarg")) {
                target.getMethod("noArgSite").invoke(null);
            } else if (which.equals("instance")) {
                target.getMethod("instanceSite", int.class).invoke(instance, 7);
            } else if (which.equals("interface")) {
                target.getMethod("interfaceSite", int.class).invoke(instance, 4);
            } else if (which.equals("interfacecancel")) {
                target.getMethod("interfaceCancelSite", int.class).invoke(instance, 6);
            } else if (which.equals("cancel")) {
                target.getMethod("cancelSite", int.class).invoke(null, 9);
            } else if (which.equals("twice")) {
                target.getMethod("twice", int.class).invoke(null, 3);
            } else if (which.equals("plain")) {
                target.getMethod("plain", int.class).invoke(null, 11);
            } else {
                throw new IllegalArgumentException(which);
            }
            Class<?> callback = Class.forName("demo.Callback", true, loader);
            System.out.println("calls=" + callback.getField("calls").getInt(null));
            System.out.println("last=" + callback.getField("last").getInt(null));
            System.out.println("marked=" + target.getField("marked").getInt(null));
            System.out.println("poked=" + target.getField("poked").getInt(null));
            System.out.println("after=" + target.getField("after").get(null));
        }
    }
}
"#;

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

fn compile(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-invoke-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(src.join("org/spongepowered/asm/mixin/injection/callback")).unwrap();
    fs::create_dir_all(&classes).unwrap();
    fs::write(src.join("demo/Target.java"), TARGET).unwrap();
    fs::write(src.join("demo/Sink.java"), SINK).unwrap();
    fs::write(src.join("demo/Callback.java"), CALLBACK).unwrap();
    fs::write(
        src.join("org/spongepowered/asm/mixin/injection/callback/CallbackInfo.java"),
        CALLBACK_INFO,
    )
    .unwrap();
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .arg(src.join("demo/Target.java"))
        .arg(src.join("demo/Sink.java"))
        .arg(src.join("demo/Callback.java"))
        .arg(src.join("org/spongepowered/asm/mixin/injection/callback/CallbackInfo.java"))
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

fn site(owner: &str, name: &str, descriptor: &str) -> MethodRef {
    method(owner, name, descriptor)
}

fn run(classes: &Path, target: &[u8], which: &str) -> Option<Vec<String>> {
    let java = java()?;
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-weaver-invoke-run-{which}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("demo")).unwrap();
    fs::create_dir_all(root.join("org/spongepowered/asm/mixin/injection/callback")).unwrap();
    for entry in fs::read_dir(classes.join("demo")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "class")
        {
            fs::copy(&path, root.join("demo").join(path.file_name().unwrap())).unwrap();
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
                root.join("org/spongepowered/asm/mixin/injection/callback")
                    .join(path.file_name().unwrap()),
            )
            .unwrap();
        }
    }
    fs::write(root.join("demo/Target.class"), target).unwrap();
    fs::write(root.join("Runner.java"), RUNNER).unwrap();
    let output = Command::new(java)
        .arg(root.join("Runner.java"))
        .arg(&root)
        .arg(which)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{which}: the woven class did not load or run:\n{}\n{stdout}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(stdout.lines().map(str::to_owned).collect())
}

fn value<'a>(lines: &'a [String], key: &str) -> &'a str {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} line in {lines:?}"))
}

#[test]
fn invoke_static_calls_the_callback_before_the_call() {
    let classes = compile("static");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_invoke(
        &class,
        &method("demo/Target", "staticSite", "(I)V"),
        &method("demo/Callback", "onStatic", "(I)V"),
        &site("demo/Target", "mark", "(I)V"),
        false,
    )
    .unwrap();
    let reparsed = ClassFile::parse(&woven).unwrap();
    assert_eq!(
        reparsed.write().unwrap(),
        woven,
        "the woven class round-trips"
    );
    assert!(
        woven
            .windows(b"onStatic".len())
            .any(|window| window == b"onStatic"),
        "the callback reference is in the pool"
    );

    if let Some(lines) = run(&classes, &woven, "static") {
        assert_eq!(value(&lines, "calls="), "1");
        assert_eq!(value(&lines, "last="), "5");
        assert_eq!(value(&lines, "marked="), "5");
    }
}

#[test]
fn invoke_no_argument_callback_runs_before_the_call() {
    let classes = compile("noarg");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_invoke(
        &class,
        &method("demo/Target", "noArgSite", "()V"),
        &method("demo/Callback", "onNoArg", "()V"),
        &site("demo/Target", "mark", "(I)V"),
        false,
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the woven class round-trips"
    );

    if let Some(lines) = run(&classes, &woven, "noarg") {
        assert_eq!(value(&lines, "calls="), "1");
        assert_eq!(value(&lines, "last="), "42");
        assert_eq!(value(&lines, "marked="), "1");
    }
}

#[test]
fn invoke_interface_sites_are_supported() {
    let classes = compile("interface");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_invoke(
        &class,
        &method("demo/Target", "interfaceSite", "(I)V"),
        &method("demo/Callback", "onStatic", "(I)V"),
        &site("demo/Sink", "accept", "(I)V"),
        false,
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the woven class round-trips"
    );
    if let Some(lines) = run(&classes, &woven, "interface") {
        assert_eq!(value(&lines, "calls="), "1");
        assert_eq!(value(&lines, "last="), "4");
        assert_eq!(value(&lines, "poked="), "4");
    }
}

#[test]
fn cancellable_invoke_interface_pops_the_receiver() {
    let classes = compile("interface-cancel");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_invoke(
        &class,
        &method("demo/Target", "interfaceCancelSite", "(I)V"),
        &method(
            "demo/Callback",
            "onCancel",
            "(ILorg/spongepowered/asm/mixin/injection/callback/CallbackInfo;)V",
        ),
        &site("demo/Sink", "accept", "(I)V"),
        true,
    )
    .unwrap();
    if let Some(lines) = run(&classes, &woven, "interfacecancel") {
        assert_eq!(value(&lines, "calls="), "1");
        assert_eq!(value(&lines, "last="), "6");
        assert_eq!(
            value(&lines, "poked="),
            "0",
            "the interface call was cancelled"
        );
        assert_eq!(value(&lines, "after="), "iface");
    }
}

#[test]
fn invoke_instance_receives_the_woven_method_receiver() {
    let classes = compile("instance");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let woven = weave_invoke(
        &class,
        &method("demo/Target", "instanceSite", "(I)V"),
        &method("demo/Callback", "onInstance", "(Ldemo/Target;I)V"),
        &site("demo/Target", "poke", "(I)V"),
        false,
    )
    .unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the woven class round-trips"
    );

    if let Some(lines) = run(&classes, &woven, "instance") {
        assert_eq!(value(&lines, "calls="), "1");
        assert_eq!(value(&lines, "last="), "7");
        assert_eq!(value(&lines, "poked="), "7");
        assert_eq!(value(&lines, "marked="), "0");
    }
}

#[test]
fn cancellable_invoke_suppresses_only_the_call() {
    let classes = compile("cancel");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let plan = MixinInjectionPlan {
        target: method("demo/Target", "cancelSite", "(I)V"),
        callback: method(
            "demo/Callback",
            "onCancel",
            "(ILorg/spongepowered/asm/mixin/injection/callback/CallbackInfo;)V",
        ),
        at: "INVOKE".into(),
        cancellable: true,
        invoke_target: Some(site("demo/Target", "mark", "(I)V")),
        ordinal: None,
        slice: None,
        shift: None,
    };
    let woven = weave_invoke_plan(&class, &plan).unwrap();
    assert_eq!(
        ClassFile::parse(&woven).unwrap().write().unwrap(),
        woven,
        "the woven class round-trips"
    );
    // The plan-driven whole-class path applies the same INVOKE edit and reports
    // the synthetic `CallbackInfo` a cancellable insert references.
    let (from_class, added) =
        horizon_native_java::weave_class_with_extras(&class, std::slice::from_ref(&plan)).unwrap();
    assert_eq!(from_class, woven);
    assert!(added.contains_key("org/spongepowered/asm/mixin/injection/callback/CallbackInfo"));

    if let Some(lines) = run(&classes, &woven, "cancel") {
        assert_eq!(value(&lines, "calls="), "1");
        assert_eq!(value(&lines, "last="), "9");
        assert_eq!(value(&lines, "marked="), "0", "ci.cancel() suppressed mark");
        assert_eq!(
            value(&lines, "after="),
            "done",
            "the woven method kept running"
        );
    }
}

#[test]
fn unwoven_control_is_unchanged() {
    let classes = compile("control");
    let bytes = read_class(&classes, "demo/Target");
    if let Some(lines) = run(&classes, &bytes, "plain") {
        assert_eq!(value(&lines, "calls="), "0");
        assert_eq!(value(&lines, "marked="), "11");
    }
}

#[test]
fn non_void_cancellable_sites_stay_rejected() {
    let classes = compile("non-void-cancel");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();
    let error = weave_invoke(
        &class,
        &method("demo/Target", "valueSite", "(I)I"),
        &method(
            "demo/Callback",
            "onCancel",
            "(ILorg/spongepowered/asm/mixin/injection/callback/CallbackInfo;)V",
        ),
        &site("demo/Target", "value", "(I)I"),
        true,
    )
    .unwrap_err();
    assert!(error.contains("bounded to void call sites"), "{error}");
}

#[test]
fn invokespecial_and_ambiguity_stay_rejected() {
    let classes = compile("rejections");
    let bytes = read_class(&classes, "demo/Target");
    let class = ClassFile::parse(&bytes).unwrap();

    let special = weave_invoke(
        &class,
        &method("demo/Target", "make", "()V"),
        &method("demo/Callback", "onMake", "()V"),
        &site("demo/Target", "<init>", "()V"),
        false,
    )
    .unwrap_err();
    assert!(special.contains("invokespecial"), "{special}");

    let missing = weave_invoke(
        &class,
        &method("demo/Target", "staticSite", "(I)V"),
        &method("demo/Callback", "onStatic", "(I)V"),
        &site("demo/Target", "missing", "(I)V"),
        false,
    )
    .unwrap_err();
    assert!(missing.contains("found no"), "{missing}");

    let wrong = weave_invoke(
        &class,
        &method("demo/Target", "staticSite", "(I)V"),
        &method("demo/Callback", "wrong", "(Ljava/lang/String;)V"),
        &site("demo/Target", "mark", "(I)V"),
        false,
    )
    .unwrap_err();
    assert!(wrong.contains("static target expects"), "{wrong}");

    let twice = weave_invoke(
        &class,
        &method("demo/Target", "twice", "(I)V"),
        &method(
            "demo/Callback",
            "onCancel",
            "(ILorg/spongepowered/asm/mixin/injection/callback/CallbackInfo;)V",
        ),
        &site("demo/Target", "mark", "(I)V"),
        true,
    )
    .unwrap_err();
    assert!(twice.contains("exactly one matching call site"), "{twice}");

    // `weave_class` applies INVOKE plans through the same selected call-site
    // path as the dedicated entry point.
    let plan = MixinInjectionPlan::inject_at_invoke(
        method("demo/Target", "staticSite", "(I)V"),
        method("demo/Callback", "onStatic", "(I)V"),
        site("demo/Target", "mark", "(I)V"),
    );
    let woven = horizon_native_java::weave_class(&class, std::slice::from_ref(&plan)).unwrap();
    assert_ne!(woven, bytes);
    assert_eq!(woven, weave_invoke_plan(&class, &plan).unwrap());
}

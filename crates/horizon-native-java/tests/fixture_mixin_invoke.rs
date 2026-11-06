//! Runtime-built Mixin fixture for `@At("INVOKE")`.
//!
//! The jar comes from `javac` so discovery reads the real
//! `RuntimeVisibleAnnotations` encoding of `@At(value = "INVOKE", target =
//! ...)`; the tests then call `execute_discovered_mixins` with no hand-built
//! plans.  `demo/Probe` exists only at compile time: leaving it out of the jar
//! makes every `Probe` call cross the host boundary, where `StdHost` records
//! invocation order.
//!
//! The woven body calls two matching stubs (`mark("a")`, `mark("b")`) and one
//! non-matching one (`other(value)`), so a plan that hooks
//! `Ldemo/Probe;mark(Ljava/lang/String;)V` must run twice, in call order, and
//! leave `other` alone.

use horizon_native_java::{MethodRef, NativeJar, StdHost, Value};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Command};
use zip::write::SimpleFileOptions;

fn javac() -> std::path::PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn files_under(root: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

fn build_jar(
    name: &str,
    sources: BTreeMap<&str, &str>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-invoke-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let mut paths = Vec::new();
    for (relative, body) in sources {
        let path = src.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        paths.push(path);
    }
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(paths)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed for {name}");

    let jar_path = root.join(format!("{name}.jar"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(br#"{"id":"mixin-invoke-fixture","version":"1.0"}"#)
        .unwrap();
    for class in files_under(&classes) {
        let entry = class
            .strip_prefix(&classes)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // `demo/Probe` stays outside the jar so its calls are host-visible.
        if entry == "demo/Probe.class" {
            continue;
        }
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(&fs::read(class).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    (root, jar_path)
}

fn target(name: &str, descriptor: &str) -> MethodRef {
    MethodRef {
        owner: "demo/Target".into(),
        name: name.into(),
        descriptor: descriptor.into(),
    }
}

/// Every `demo/Probe` call the host recorded, as `(method, first argument)`.
fn probe_calls(host: &StdHost) -> Vec<(String, Value)> {
    host.invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .map(|invocation| {
            (
                invocation.method.name.clone(),
                invocation.args.first().cloned().unwrap_or(Value::Void),
            )
        })
        .collect()
}

fn text(value: &str) -> Value {
    Value::Ref(Some(value.into()))
}

const INVOKE_MIXIN_PLAIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class InvokeMixin {
    @Inject(method = "run(Ljava/lang/String;)V",
            at = @At(value = "INVOKE", target = "Ldemo/Probe;mark(Ljava/lang/String;)V"))
    private static void onMark(String value, CallbackInfo info) {
        Probe.note("cb", value);
    }
}
"#;

const INVOKE_MIXIN_CANCEL: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class InvokeMixin {
    @Inject(method = "run(Ljava/lang/String;)V",
            at = @At(value = "INVOKE", target = "Ldemo/Probe;mark(Ljava/lang/String;)V"),
            cancellable = true)
    private static void onMark(String value, CallbackInfo info) {
        Probe.note("cb", value);
        info.cancel();
    }
}
"#;

fn fixture_sources(mixin: &'static str) -> BTreeMap<&'static str, &'static str> {
    let mut sources = BTreeMap::new();
    sources.insert(
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
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/At.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface At {
    String value() default "";
    String target() default "";
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/Inject.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Inject {
    String[] method();
    At[] at();
    boolean cancellable() default false;
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.java",
        r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfo {
    public void cancel() {}
}
"#,
    );
    sources.insert(
        "demo/Probe.java",
        r#"package demo;
public final class Probe {
    public static void mark(String marker) {}
    public static void other(String value) {}
    public static void note(String marker, String value) {}
}
"#,
    );
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public final class Target {
    public static void run(String value) {
        Probe.mark("a");
        Probe.mark("b");
        Probe.other(value);
    }
}
"#,
    );
    sources.insert("demo/InvokeMixin.java", mixin);
    sources
}

#[test]
fn executes_discovered_invoke_callback_at_every_matching_call() {
    let (root, jar_path) = build_jar("plain", fixture_sources(INVOKE_MIXIN_PLAIN));
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let plans = jar.mixin_injection_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    let plan = &plans[0];
    assert_eq!(plan.at, "INVOKE");
    assert!(!plan.cancellable);
    assert_eq!(
        plan.invoke_target,
        Some(MethodRef {
            owner: "demo/Probe".into(),
            name: "mark".into(),
            descriptor: "(Ljava/lang/String;)V".into(),
        })
    );
    assert_eq!(
        plan.target,
        MethodRef {
            owner: "demo/Target".into(),
            name: "run".into(),
            descriptor: "(Ljava/lang/String;)V".into(),
        }
    );
    assert!(jar.unresolved_invoke_plans().is_empty());

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(
            &target("run", "(Ljava/lang/String;)V"),
            None,
            &[text("hello")],
            &mut host,
        )
        .unwrap();
    assert_eq!(report.target_result, Value::Void);
    assert!(!report.cancelled);
    assert_eq!(
        report.callback_results,
        vec![Value::Void, Value::Void],
        "one callback result per matching call"
    );
    assert_eq!(
        report.matched_invoke_sites, 2,
        "the two sibling calls are two distinct sites"
    );
    assert_eq!(
        probe_calls(&host),
        vec![
            ("note".into(), text("cb")),
            ("mark".into(), text("a")),
            ("note".into(), text("cb")),
            ("mark".into(), text("b")),
            // `other` is a different name, so the hook leaves it alone.
            ("other".into(), text("hello")),
        ],
        "the callback runs before each matching call, in call order"
    );
    // The callback mirrors the woven method's arguments.
    let mirrored: Vec<Value> = host
        .invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .filter(|invocation| invocation.method.name == "note")
        .filter_map(|invocation| invocation.args.get(1).cloned())
        .collect();
    assert_eq!(mirrored, vec![text("hello"), text("hello")]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn cancelled_invoke_suppresses_the_call_and_keeps_the_target_running() {
    let (root, jar_path) = build_jar("cancel", fixture_sources(INVOKE_MIXIN_CANCEL));
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let plans = jar.mixin_injection_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert!(plans[0].cancellable);

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(
            &target("run", "(Ljava/lang/String;)V"),
            None,
            &[text("hello")],
            &mut host,
        )
        .unwrap();
    assert_eq!(report.target_result, Value::Void);
    assert!(report.cancelled, "ci.cancel() suppresses the matched calls");
    assert_eq!(report.callback_results, vec![Value::Void, Value::Void]);
    assert_eq!(report.matched_invoke_sites, 2);
    assert_eq!(
        probe_calls(&host),
        vec![
            ("note".into(), text("cb")),
            ("note".into(), text("cb")),
            // The target body still ran its non-matching call.
            ("other".into(), text("hello")),
        ],
        "both stubs are suppressed and the target keeps running"
    );

    let _ = fs::remove_dir_all(root);
}

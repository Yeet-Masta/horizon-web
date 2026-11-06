//! Runtime-built Mixin fixture for annotation-to-execution wiring.
//!
//! The jar comes from `javac` so discovery reads the real
//! `RuntimeVisibleAnnotations` encoding; the fixtures then call
//! `execute_discovered_mixins` with no hand-built plans.  `demo/Probe` exists
//! only at compile time: leaving it out of the jar makes every `Probe.mark`
//! call cross the host boundary, where `StdHost` records invocation order and
//! the mirrored arguments.

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
    metadata_path: &str,
    metadata: &[u8],
    excluded: &[&str],
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-execution-{name}-{}",
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
    zip.start_file(metadata_path, SimpleFileOptions::default())
        .unwrap();
    zip.write_all(metadata).unwrap();
    for class in files_under(&classes) {
        let entry = class
            .strip_prefix(&classes)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if excluded.contains(&entry.as_str()) {
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

/// Markers recorded at the host boundary, in invocation order.
fn mark_order(host: &StdHost) -> Vec<String> {
    host.invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .filter_map(|invocation| match invocation.args.first() {
            Some(Value::Ref(Some(marker))) => Some(marker.clone()),
            _ => None,
        })
        .collect()
}

fn fixture_sources() -> BTreeMap<&'static str, &'static str> {
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
    public static void mark(String marker, int value) {}
}
"#,
    );
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public final class Target {
    public static void run(int value) { Probe.mark("target", value); }
    public static void only(int value) { Probe.mark("only-target", value); }
    public static void mixed(int value) { Probe.mark("mixed-target", value); }
    public static void cancelMe(int value) { Probe.mark("cancel-target", value); }
}
"#,
    );
    sources.insert(
        "demo/TargetsMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class TargetsMixin {
    @Inject(method = "run(I)V", at = @At("HEAD"))
    private static void first(int value) { Probe.mark("head-first", value); }

    @Inject(method = "run", at = @At("HEAD"))
    private static void generic(int value) { Probe.mark("head-generic", value); }

    @Inject(method = "run(I)V", at = @At("HEAD"))
    private static void second(int value) { Probe.mark("head-second", value); }

    @Inject(method = "run(I)V", at = @At("HEAD"))
    private static void withInfo(int value, CallbackInfo info) { Probe.mark("head-info", value); }

    @Inject(method = "only(I)V", at = @At("RETURN"))
    private static void after(int value) { Probe.mark("return", value); }
}
"#,
    );
    sources.insert(
        "demo/RejectedMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class RejectedMixin {
    @Inject(method = "mixed(I)V", at = @At("HEAD"))
    private static void before(int value) { Probe.mark("mixed-before", value); }

    @Inject(method = "mixed(I)V", at = @At("RETURN"))
    private static void afterReturn(int value) { Probe.mark("mixed-after", value); }

    @Inject(method = "cancelMe(I)V", at = @At("HEAD"), cancellable = true)
    private static void cancel(int value, CallbackInfo info) { Probe.mark("cancel", value); }
}
"#,
    );
    sources
}

#[test]
fn executes_discovered_head_callbacks_in_plan_order_once() {
    let (root, jar_path) = build_jar(
        "execution",
        fixture_sources(),
        "fabric.mod.json",
        br#"{"id":"mixin-execution-fixture","version":"1.0"}"#,
        &["demo/Probe.class"],
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    assert_eq!(
        jar.discovered_mixin_targets()
            .iter()
            .map(|target| (
                target.owner.as_str(),
                target.name.as_str(),
                target.descriptor.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("demo/Target", "cancelMe", "(I)V"),
            ("demo/Target", "mixed", "(I)V"),
            ("demo/Target", "only", "(I)V"),
            // The descriptor-less injection names the `run` target without an
            // overload, so it appears separately from the concrete one.
            ("demo/Target", "run", ""),
            ("demo/Target", "run", "(I)V"),
        ]
    );

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("run", "(I)V"), None, &[Value::Int(7)], &mut host)
        .unwrap();
    assert_eq!(report.target_result, Value::Void);
    assert_eq!(
        report.callback_results,
        vec![Value::Void, Value::Void, Value::Void, Value::Void]
    );
    assert_eq!(report.callback_result, Value::Void);
    // Mixin applies one class's injectors in method declaration order, so
    // `first` (declared before the descriptor-less `generic`) runs first.
    assert_eq!(
        mark_order(&host),
        vec![
            "head-first",
            "head-generic",
            "head-second",
            "head-info",
            "target"
        ]
    );
    assert_eq!(
        mark_order(&host)
            .iter()
            .filter(|mark| *mark == "target")
            .count(),
        1,
        "the target body runs exactly once"
    );
    let mirrored: Vec<Value> = host
        .invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .filter_map(|invocation| invocation.args.get(1).cloned())
        .collect();
    assert_eq!(mirrored.len(), 5);
    assert!(
        mirrored.iter().all(|value| *value == Value::Int(7)),
        "callbacks mirror the target arguments: {mirrored:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn executes_discovered_return_callback_after_the_target() {
    let (root, jar_path) = build_jar(
        "execution-return",
        fixture_sources(),
        "fabric.mod.json",
        br#"{"id":"mixin-execution-return-fixture","version":"1.0"}"#,
        &["demo/Probe.class"],
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("only", "(I)V"), None, &[Value::Int(9)], &mut host)
        .unwrap();
    assert_eq!(report.callback_results, vec![Value::Void]);
    assert_eq!(report.target_result, Value::Void);
    assert_eq!(mark_order(&host), vec!["only-target", "return"]);
    assert_eq!(
        host.invocations()
            .iter()
            .filter(|invocation| invocation.method.owner == "demo/Probe")
            .filter_map(|invocation| invocation.args.get(1).cloned())
            .collect::<Vec<_>>(),
        vec![Value::Int(9), Value::Int(9)]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn executes_head_and_return_callbacks_on_one_target() {
    let (root, jar_path) = build_jar(
        "execution-rejected",
        fixture_sources(),
        "fabric.mod.json",
        br#"{"id":"mixin-execution-rejected-fixture","version":"1.0"}"#,
        &["demo/Probe.class"],
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    // HEAD and RETURN callbacks share one target: the HEAD runs before the
    // single target body and the RETURN after it, as one woven method does.
    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("mixed", "(I)V"), None, &[Value::Int(1)], &mut host)
        .unwrap();
    assert!(!report.cancelled);
    assert!(!report.return_value_overridden);
    assert_eq!(report.target_result, Value::Void);
    assert_eq!(report.callback_results, vec![Value::Void, Value::Void]);
    assert_eq!(
        mark_order(&host),
        vec!["mixed-before", "mixed-target", "mixed-after"],
        "HEAD, then the target once, then RETURN"
    );
    assert_eq!(
        mark_order(&host)
            .iter()
            .filter(|mark| *mark == "mixed-target")
            .count(),
        1,
        "the target body runs exactly once"
    );

    // A cancellable HEAD plan is accepted; this callback never calls
    // `CallbackInfo.cancel()`, so the target still runs once.
    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(
            &target("cancelMe", "(I)V"),
            None,
            &[Value::Int(1)],
            &mut host,
        )
        .unwrap();
    assert!(!report.cancelled);
    assert_eq!(mark_order(&host), vec!["cancel", "cancel-target"]);

    let _ = fs::remove_dir_all(root);
}

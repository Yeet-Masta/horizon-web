//! Runtime-built Mixin fixture for HEAD and RETURN callbacks on one target.
//!
//! Real Mixin runs both anchors for one woven method: HEAD before the body,
//! RETURN/TAIL after it with the target's result.  The jar comes from `javac`,
//! so the annotations are real bytecode.  `demo/Probe`, `CallbackInfo` and
//! `CallbackInfoReturnable` stay outside the jar, so invocation order is
//! recorded at the host boundary and the callback-info methods resolve against
//! the runtime's own table.

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
        "horizon-native-java-mixin-mixed-{name}-{}",
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
    zip.write_all(br#"{"id":"mixin-mixed-fixture","version":"1.0"}"#)
        .unwrap();
    let excluded = [
        "demo/Probe.class",
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.class",
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable.class",
    ];
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

/// Probe markers recorded at the host boundary, in invocation order.
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
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable.java",
        r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfoReturnable<T> {
    private T returnValue;
    public void setReturnValue(T value) { this.returnValue = value; }
    public T getReturnValue() { return returnValue; }
}
"#,
    );
    sources.insert(
        "demo/Probe.java",
        r#"package demo;
public final class Probe {
    public static void mark(String marker) {}
}
"#,
    );
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public final class Target {
    public static int compute() { Probe.mark("target"); return 10; }
    public static int cancelled() { Probe.mark("cancel-target"); return 10; }
}
"#,
    );
    // One mixin carries both anchors for `compute`: HEAD before the body and
    // RETURN after it, where the callback reads the actual result and then
    // replaces it.
    sources.insert(
        "demo/ComputeMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class ComputeMixin {
    @Inject(method = "compute()I", at = @At("HEAD"))
    private static void onHead(CallbackInfo info) { Probe.mark("head"); }

    @Inject(method = "compute()I", at = @At("RETURN"))
    private static void onReturn(CallbackInfoReturnable<Integer> info) {
        Probe.mark(String.valueOf(info.getReturnValue()));
        info.setReturnValue(42);
    }
}
"#,
    );
    // Same two anchors, but the HEAD callback cancels: the target body and the
    // RETURN callback must not run.
    sources.insert(
        "demo/CancelMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class CancelMixin {
    @Inject(method = "cancelled()I", at = @At("HEAD"), cancellable = true)
    private static void onHead(CallbackInfo info) {
        Probe.mark("cancel-head");
        info.cancel();
    }

    @Inject(method = "cancelled()I", at = @At("RETURN"))
    private static void onReturn(CallbackInfoReturnable<Integer> info) {
        Probe.mark("cancel-return");
        info.setReturnValue(1);
    }
}
"#,
    );
    sources
}

fn fixture(name: &str) -> (std::path::PathBuf, NativeJar) {
    let (root, jar_path) = build_jar(name, fixture_sources());
    let jar = NativeJar::from_path(&jar_path).unwrap();
    (root, jar)
}

#[test]
fn head_runs_before_the_target_and_return_after_it_with_the_actual_result() {
    let (root, jar) = fixture("override");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("compute", "()I"), None, &[], &mut host)
        .unwrap();
    assert!(!report.cancelled);
    assert!(
        report.return_value_overridden,
        "the RETURN callback replaced the target value"
    );
    assert_eq!(report.target_result, Value::Int(42));
    assert_eq!(
        report.callback_results,
        vec![Value::Void, Value::Void],
        "one HEAD and one RETURN callback ran"
    );
    // The RETURN callback reads 10, the value the target returned, before
    // replacing it.
    assert_eq!(mark_order(&host), vec!["head", "target", "10"]);
    assert_eq!(
        mark_order(&host)
            .iter()
            .filter(|mark| *mark == "target")
            .count(),
        1,
        "the target body runs exactly once"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_cancelled_head_skips_the_target_and_the_return_callback() {
    let (root, jar) = fixture("cancel");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("cancelled", "()I"), None, &[], &mut host)
        .unwrap();
    assert!(report.cancelled);
    assert!(
        !report.return_value_overridden,
        "the RETURN callback never ran, so nothing overrode the value"
    );
    assert_eq!(
        report.target_result,
        Value::Int(0),
        "a cancelled plan answers the target descriptor's default"
    );
    assert_eq!(report.callback_results, vec![Value::Void]);
    assert_eq!(
        mark_order(&host),
        vec!["cancel-head"],
        "the target body and the RETURN callback are skipped"
    );

    let _ = fs::remove_dir_all(root);
}

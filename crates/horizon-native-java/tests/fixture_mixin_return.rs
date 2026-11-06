//! Runtime-built Mixin fixture for `CallbackInfoReturnable`.
//!
//! The jar comes from `javac`, so the callbacks are real bytecode: they read
//! the target's result with `getReturnValue()` and replace it with
//! `setReturnValue(...)`.  `demo/Probe`, `CallbackInfo` and
//! `CallbackInfoReturnable` stay outside the jar, so the probe call crosses the
//! host boundary where `StdHost` records order, and the returnable methods
//! resolve against the runtime's own table.

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
        "horizon-native-java-mixin-return-{name}-{}",
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
    zip.write_all(br#"{"id":"mixin-return-fixture","version":"1.0"}"#)
        .unwrap();
    // The probe and the callback info stay outside the jar, so their calls take
    // the same boundary a real mod's calls do.
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
    boolean cancelled;
    public void cancel() { cancelled = true; }
    public boolean isCancelled() { return cancelled; }
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
    public static int number() { Probe.mark("number-target"); return 5; }
    public static String text() { Probe.mark("text-target"); return "actual"; }
    public static int tail() { Probe.mark("tail-target"); return 5; }
}
"#,
    );
    // Discovery sorts callbacks by owning class, so AOverride runs before
    // BObserve at the same RETURN point.
    sources.insert(
        "demo/AOverrideMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class AOverrideMixin {
    @Inject(method = "number()I", at = @At("RETURN"), cancellable = true)
    private static void onNumber(CallbackInfoReturnable<Integer> info) {
        Probe.mark("number-override");
        info.setReturnValue(7);
    }
}
"#,
    );
    sources.insert(
        "demo/BObserveMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class BObserveMixin {
    @Inject(method = "number()I", at = @At("RETURN"))
    private static void onNumber(CallbackInfoReturnable<Integer> info) {
        Probe.mark(String.valueOf(info.getReturnValue()));
    }
}
"#,
    );
    sources.insert(
        "demo/TextMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class TextMixin {
    @Inject(method = "text()Ljava/lang/String;", at = @At("RETURN"))
    private static void onText(CallbackInfoReturnable<String> info) {
        Probe.mark(String.valueOf(info.getReturnValue()));
    }
}
"#,
    );
    sources.insert(
        "demo/TailMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class TailMixin {
    @Inject(method = "tail()I", at = @At("TAIL"))
    private static void onTail(CallbackInfoReturnable<Integer> info) {
        Probe.mark("tail-override");
        info.setReturnValue(9);
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
fn a_return_injection_overrides_the_target_value_and_the_last_callback_sees_it() {
    let (root, jar) = fixture("override");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("number", "()I"), None, &[], &mut host)
        .unwrap();
    assert!(
        report.return_value_overridden,
        "setReturnValue replaced the target value"
    );
    assert!(
        !report.cancelled,
        "cancellable RETURN cannot un-run the body"
    );
    assert_eq!(report.target_result, Value::Int(7));
    assert_eq!(report.callback_results, vec![Value::Void, Value::Void]);
    assert_eq!(
        mark_order(&host),
        vec!["number-target", "number-override", "7"],
        "the target runs once, then the overriding callback, then the observer that reads the override"
    );
    assert_eq!(
        mark_order(&host)
            .iter()
            .filter(|mark| *mark == "number-target")
            .count(),
        1,
        "the target body runs exactly once"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_return_injection_without_an_override_returns_the_target_value() {
    let (root, jar) = fixture("passthrough");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(
            &target("text", "()Ljava/lang/String;"),
            None,
            &[],
            &mut host,
        )
        .unwrap();
    assert!(!report.return_value_overridden);
    assert_eq!(report.target_result, Value::Ref(Some("actual".into())));
    // `getReturnValue` hands the callback the target's own result until a
    // `setReturnValue` replaces it.
    assert_eq!(mark_order(&host), vec!["text-target", "actual"]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_tail_injection_overrides_the_target_value() {
    let (root, jar) = fixture("tail");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("tail", "()I"), None, &[], &mut host)
        .unwrap();
    assert!(report.return_value_overridden);
    assert_eq!(report.target_result, Value::Int(9));
    assert_eq!(mark_order(&host), vec!["tail-target", "tail-override"]);

    // The single-plan entry point reports the same contract.
    let plan = jar
        .mixin_injection_plans()
        .into_iter()
        .find(|plan| plan.target.name == "tail")
        .unwrap();
    let mut host = StdHost::new();
    let report = jar
        .execute_mixin_at_return(&plan, None, &[], &mut host)
        .unwrap();
    assert!(report.return_value_overridden);
    assert_eq!(report.target_result, Value::Int(9));
    assert_eq!(mark_order(&host), vec!["tail-target", "tail-override"]);

    let _ = fs::remove_dir_all(root);
}

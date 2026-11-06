//! Runtime-built Mixin fixture for `CallbackInfo` cancellation.
//!
//! The jar comes from `javac`, so the callback bodies are real bytecode that
//! calls `ci.cancel()` and `ci.isCancelled()` on the object the native
//! execution handed them, and the `@Inject(cancellable = true)` flag is read
//! from the real `RuntimeVisibleAnnotations` encoding.  `demo/Probe` and
//! `CallbackInfo` are left out of the jar: the probe call crosses the host
//! boundary, where `StdHost` records the order, and the callback-info call
//! resolves against the runtime's own table.

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
        "horizon-native-java-mixin-cancel-{name}-{}",
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
    zip.write_all(br#"{"id":"mixin-cancel-fixture","version":"1.0"}"#)
        .unwrap();
    // The probe and the callback info stay outside the jar, so their calls take
    // the same boundary a real mod's calls do.
    let excluded = [
        "demo/Probe.class",
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.class",
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

/// The two target bodies both return 5 and record a marker, so a cancelled call
/// is distinguishable from a body that ran.
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
    public static int cancelMe() { Probe.mark("cancel-target"); return 5; }
    public static int keep() { Probe.mark("keep-target"); return 5; }
}
"#,
    );
    sources.insert(
        "demo/CancelMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class CancelMixin {
    @Inject(method = "cancelMe()I", at = @At("HEAD"), cancellable = true)
    private static void cancelHead(CallbackInfo info) {
        Probe.mark("cancel-head");
        info.cancel();
        Probe.mark(info.isCancelled() ? "cancel-seen" : "cancel-missed");
    }
}
"#,
    );
    sources.insert(
        "demo/KeepMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class KeepMixin {
    @Inject(method = "keep()I", at = @At("HEAD"), cancellable = true)
    private static void keepHead(CallbackInfo info) {
        Probe.mark(info.isCancelled() ? "keep-cancelled" : "keep-head");
    }
}
"#,
    );
    sources.insert(
        "demo/LateMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(value = Target.class, targets = {"Ldemo/Target;"})
public abstract class LateMixin {
    @Inject(method = "cancelMe()I", at = @At("HEAD"))
    private static void lateHead(CallbackInfo info) {
        Probe.mark("cancel-late");
    }
}
"#,
    );
    sources
}

/// Build the fixture under `name`; each test uses its own directory so the
/// parallel run never shares a scratch tree.
fn fixture(name: &str) -> (std::path::PathBuf, NativeJar) {
    let (root, jar_path) = build_jar(name, fixture_sources());
    let jar = NativeJar::from_path(&jar_path).unwrap();
    (root, jar)
}

#[test]
fn a_cancelled_head_injection_skips_the_target_body() {
    let (root, jar) = fixture("cancel");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("cancelMe", "()I"), None, &[], &mut host)
        .unwrap();
    assert!(report.cancelled, "the callback cancelled the injection");
    assert_eq!(report.callback_results, vec![Value::Void]);
    // The woven method returns the descriptor's default without running the
    // body, which would have returned 5 and marked `cancel-target`.
    assert_eq!(report.target_result, Value::Int(0));
    assert_eq!(
        mark_order(&host),
        vec!["cancel-head", "cancel-seen"],
        "isCancelled reports the flag cancel() set, the later mixin never ran, and so did no target body"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_cancellable_injection_that_does_not_cancel_still_runs_the_target() {
    let (root, jar) = fixture("keep");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target("keep", "()I"), None, &[], &mut host)
        .unwrap();
    assert!(!report.cancelled);
    assert_eq!(report.target_result, Value::Int(5));
    assert_eq!(
        mark_order(&host),
        vec!["keep-head", "keep-target"],
        "the target body runs once when no callback cancels"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_single_plan_and_chain_entry_points_honour_cancellation() {
    let (root, jar) = fixture("entry-points");

    let plans = jar
        .mixin_injection_plans()
        .into_iter()
        .filter(|plan| plan.target.name == "cancelMe")
        .collect::<Vec<_>>();
    assert_eq!(plans.len(), 2, "two HEAD mixins target cancelMe");
    assert!(plans[0].cancellable, "the cancelling plan sorts first");

    let mut host = StdHost::new();
    let report = jar
        .execute_mixin_at_head(&plans[0], None, &[], &mut host)
        .unwrap();
    assert!(report.cancelled);
    assert_eq!(report.target_result, Value::Int(0));
    assert_eq!(mark_order(&host), vec!["cancel-head", "cancel-seen"]);

    // The chain stops at the cancelling plan: the second one never runs.
    let mut host = StdHost::new();
    let report = jar
        .execute_mixin_chain(&plans, None, &[], &mut host)
        .unwrap();
    assert!(report.cancelled);
    assert_eq!(report.callback_results, vec![Value::Void]);
    assert_eq!(report.target_result, Value::Int(0));
    assert_eq!(mark_order(&host), vec!["cancel-head", "cancel-seen"]);

    let _ = fs::remove_dir_all(root);
}

//! Runtime-built Mixin fixture for `FIELD`, `NEW`, `JUMP` and `@Redirect`
//! anchors.
//!
//! Every jar comes from `javac`, so discovery reads the real
//! `RuntimeVisibleAnnotations` encoding instead of hand-built plans.  `demo/Probe`
//! stays outside the jar: its calls cross the host boundary where [`StdHost`]
//! records callback order and the values callbacks saw.
//!
//! The fixtures cover:
//!
//! * a field read and a field write, including cancellation (a cancelled read
//!   answers the descriptor default, a cancelled write is suppressed);
//! * a `new` site, including cancellation skipping the constructor;
//! * a conditional `JUMP` selected by opcode/ordinal and by no selector at all,
//!   including cancellation falling through;
//! * `@Redirect` at `HEAD`, `INVOKE` and `FIELD` (read and write);
//! * selectors and callback shapes outside the bounded forms staying report-only.

use horizon_native_java::{MethodRef, NativeJar, StdHost, Value};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Command};
use zip::write::SimpleFileOptions;

fn javac() -> std::path::PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

/// Exactly one JDK is needed for the whole fixture family; every test skips
/// when `javac -version` cannot run.
fn has_jdk() -> bool {
    Command::new(javac())
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
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
    sources: BTreeMap<&'static str, &'static str>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-anchors-{name}-{}",
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
    zip.write_all(br#"{"id":"mixin-anchors-fixture","version":"1.0"}"#)
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

fn text(value: &Value) -> String {
    match value {
        Value::Ref(Some(text)) => text.clone(),
        Value::Int(value) => value.to_string(),
        Value::Long(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Ref(None) => "null".into(),
        Value::Void => "void".into(),
    }
}

/// Every `demo/Probe.note(marker, value)` the host recorded, in order.
fn notes(host: &StdHost) -> Vec<(String, String)> {
    host.invocations()
        .iter()
        .filter(|invocation| {
            invocation.method.owner == "demo/Probe"
                && (invocation.method.name == "note" || invocation.method.name == "noteInt")
        })
        .map(|invocation| {
            (
                text(invocation.args.first().unwrap_or(&Value::Void)),
                text(invocation.args.get(1).unwrap_or(&Value::Void)),
            )
        })
        .collect()
}

const MIXIN: &str = r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface Mixin {
    Class<?>[] value() default {};
    String[] targets() default {};
}
"#;

// `At` mirrors the parts of Mixin's annotation this bounded runtime reads:
// value, target, opcode and ordinal.
const AT: &str = r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface At {
    String value() default "";
    String target() default "";
    int opcode() default 0;
    int ordinal() default -1;
}
"#;

const INJECT: &str = r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Inject {
    String[] method();
    At[] at();
    boolean cancellable() default false;
}
"#;

const REDIRECT: &str = r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Redirect {
    String[] method();
    At[] at();
}
"#;

const CALLBACK_INFO: &str = r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfo {
    public void cancel() {}
    public boolean isCancelled() { return false; }
}
"#;

const PROBE: &str = r#"package demo;
public final class Probe {
    public static int value() { return 5; }
    public static void note(String marker, String value) {}
    public static void noteInt(String marker, int value) {}
}
"#;

const TARGET: &str = r#"package demo;
public final class Target {
    static int count = 5;
    static int sink;
    public static int read() { return count; }
    public static int writeThenRead(int value) { sink = value; return peek(); }
    static int peek() { return sink; }
    public static int choose(int value) {
        if (value == 0) {
            return 10;
        }
        return 20;
    }
    public static int call() { return Probe.value(); }
    public static int whole(int value) {
        Probe.note("body", "");
        return value + 1;
    }
    public static void make() {
        new Made();
        Probe.note("after", "");
    }
}
"#;

const MADE: &str = r#"package demo;
public final class Made {
    public Made() { Probe.note("ctor", ""); }
}
"#;

fn sources(mixin_name: &'static str, mixin: &'static str) -> BTreeMap<&'static str, &'static str> {
    let mut sources = BTreeMap::new();
    sources.insert("org/spongepowered/asm/mixin/Mixin.java", MIXIN);
    sources.insert("org/spongepowered/asm/mixin/injection/At.java", AT);
    sources.insert("org/spongepowered/asm/mixin/injection/Inject.java", INJECT);
    sources.insert(
        "org/spongepowered/asm/mixin/injection/Redirect.java",
        REDIRECT,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.java",
        CALLBACK_INFO,
    );
    sources.insert("demo/Probe.java", PROBE);
    sources.insert("demo/Target.java", TARGET);
    sources.insert("demo/Made.java", MADE);
    sources.insert(mixin_name, mixin);
    sources
}

fn load(
    name: &'static str,
    mixin_name: &'static str,
    mixin: &'static str,
) -> (NativeJar, std::path::PathBuf) {
    let (root, jar_path) = build_jar(name, sources(mixin_name, mixin));
    (NativeJar::from_path(&jar_path).unwrap(), root)
}

const FIELD_READ: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class FieldReadMixin {
    @Inject(method = "read()I", at = @At(value = "FIELD", target = "Ldemo/Target;count:I"))
    private static void onRead(CallbackInfo info) { Probe.note("read", "seen"); }
}
"#;

const FIELD_READ_CANCEL: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class FieldReadCancelMixin {
    @Inject(method = "read()I", at = @At(value = "FIELD", target = "Ldemo/Target;count:I"), cancellable = true)
    private static void onRead(CallbackInfo info) { Probe.note("read", "cancel"); info.cancel(); }
}
"#;

const FIELD_WRITE: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class FieldWriteMixin {
    @Inject(method = "writeThenRead(I)I", at = @At(value = "FIELD", target = "Ldemo/Target;sink:I"))
    private static void onWrite(int argument, int written, CallbackInfo info) {
        Probe.noteInt("write", written);
    }
}
"#;

const FIELD_WRITE_CANCEL: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class FieldWriteCancelMixin {
    @Inject(method = "writeThenRead(I)I", at = @At(value = "FIELD", target = "Ldemo/Target;sink:I"), cancellable = true)
    private static void onWrite(int argument, int written, CallbackInfo info) {
        Probe.noteInt("write", written);
        info.cancel();
    }
}
"#;

#[test]
fn field_read_runs_the_callback_before_the_field_access() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("field-read", "FieldReadMixin.java", FIELD_READ);

    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].at, "FIELD");
    assert_eq!(plans[0].site.as_deref(), Some("Ldemo/Target;count:I"));
    assert!(!plans[0].redirect);
    assert!(jar.unresolved_anchor_plans().is_empty());
    assert_eq!(jar.mixin_anchor_report().field, 1);

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("read", "()I"), None, &[], &mut host)
        .unwrap();
    assert_eq!(report.target_result, Value::Int(5));
    assert_eq!(report.matched_anchor_sites, 1);
    assert!(!report.cancelled);
    assert_eq!(notes(&host), vec![("read".into(), "seen".into())]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn cancelled_field_read_leaves_the_descriptor_default() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load(
        "field-read-cancel",
        "FieldReadCancelMixin.java",
        FIELD_READ_CANCEL,
    );

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("read", "()I"), None, &[], &mut host)
        .unwrap();
    assert_eq!(
        report.target_result,
        Value::Int(0),
        "a cancelled read leaves the field type's default instead of reading count"
    );
    assert!(report.cancelled);
    assert_eq!(report.matched_anchor_sites, 1);
    assert_eq!(notes(&host), vec![("read".into(), "cancel".into())]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn field_write_callback_sees_the_written_value_and_can_suppress_the_write() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }

    for (name, mixin_name, mixin, expected, cancelled) in [
        (
            "field-write",
            "FieldWriteMixin.java",
            FIELD_WRITE,
            Value::Int(7),
            false,
        ),
        (
            "field-write-cancel",
            "FieldWriteCancelMixin.java",
            FIELD_WRITE_CANCEL,
            Value::Int(0),
            true,
        ),
    ] {
        let (jar, root) = load(name, mixin_name, mixin);
        let plans = jar.mixin_anchor_plans();
        assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
        assert_eq!(plans[0].at, "FIELD");

        let mut host = StdHost::new();
        let report = jar
            .execute_discovered_anchors(
                &target("writeThenRead", "(I)I"),
                None,
                &[Value::Int(7)],
                &mut host,
            )
            .unwrap();
        assert_eq!(report.target_result, expected);
        assert_eq!(report.cancelled, cancelled);
        assert_eq!(report.matched_anchor_sites, 1);
        assert_eq!(
            notes(&host),
            vec![("write".into(), "7".into())],
            "the callback sees the value the target wrote"
        );

        let _ = fs::remove_dir_all(root);
    }
}

const NEW_PLAIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class NewMixin {
    @Inject(method = "make()V", at = @At(value = "NEW", target = "Ldemo/Made;"))
    private static void onNew(CallbackInfo info) { Probe.note("new", "seen"); }
}
"#;

const NEW_CANCEL: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class NewCancelMixin {
    @Inject(method = "make()V", at = @At(value = "NEW", target = "Ldemo/Made;"), cancellable = true)
    private static void onNew(CallbackInfo info) { Probe.note("new", "cancel"); info.cancel(); }
}
"#;

#[test]
fn new_callback_runs_before_the_constructor() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("new-plain", "NewMixin.java", NEW_PLAIN);
    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].at, "NEW");
    assert_eq!(plans[0].site.as_deref(), Some("Ldemo/Made;"));

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("make", "()V"), None, &[], &mut host)
        .unwrap();
    assert_eq!(report.matched_anchor_sites, 1);
    assert_eq!(
        notes(&host),
        vec![
            ("new".into(), "seen".into()),
            ("ctor".into(), "".into()),
            ("after".into(), "".into()),
        ],
        "the callback runs before the constructor call"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn cancelled_new_skips_the_constructor_and_leaves_a_null_reference() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("new-cancel", "NewCancelMixin.java", NEW_CANCEL);
    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("make", "()V"), None, &[], &mut host)
        .unwrap();
    assert!(report.cancelled);
    assert_eq!(report.matched_anchor_sites, 1);
    assert_eq!(
        notes(&host),
        vec![("new".into(), "cancel".into()), ("after".into(), "".into()),],
        "the matching constructor is skipped; the target keeps running"
    );

    let _ = fs::remove_dir_all(root);
}

const JUMP_OPCODE: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class JumpOpcodeMixin {
    @Inject(method = "choose(I)I",
            at = @At(value = "JUMP", opcode = 154, ordinal = 0),
            cancellable = true)
    private static void onJump(int value, CallbackInfo info) {
        Probe.noteInt("jump", value);
        info.cancel();
    }
}
"#;

const JUMP_EVERY: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class JumpEveryMixin {
    @Inject(method = "choose(I)I", at = @At(value = "JUMP"), cancellable = true)
    private static void onJump(int value, CallbackInfo info) {
        Probe.noteInt("jump", value);
        info.cancel();
    }
}
"#;

#[test]
fn jump_by_opcode_and_ordinal_cancels_into_fall_through() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("jump-opcode", "JumpOpcodeMixin.java", JUMP_OPCODE);
    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].at, "JUMP");
    assert_eq!(plans[0].opcode, Some(154));
    assert_eq!(plans[0].ordinal, Some(0));

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("choose", "(I)I"), None, &[Value::Int(1)], &mut host)
        .unwrap();
    assert_eq!(
        report.target_result,
        Value::Int(10),
        "cancelling the IFNE jump falls through to the first return"
    );
    assert!(report.cancelled);
    assert_eq!(report.matched_anchor_sites, 1);
    assert_eq!(notes(&host), vec![("jump".into(), "1".into())]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn jump_without_a_target_runs_on_every_jump() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("jump-every", "JumpEveryMixin.java", JUMP_EVERY);
    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].site, None);
    assert_eq!(plans[0].opcode, None);

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("choose", "(I)I"), None, &[Value::Int(1)], &mut host)
        .unwrap();
    assert_eq!(report.target_result, Value::Int(10));
    assert!(report.cancelled);
    assert_eq!(report.matched_anchor_sites, 1);

    let _ = fs::remove_dir_all(root);
}

const REDIRECT_INVOKE: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class RedirectInvokeMixin {
    @Redirect(method = "call()I", at = @At(value = "INVOKE", target = "Ldemo/Probe;value()I"))
    private static int redirect(CallbackInfo info) {
        Probe.note("redirect", "invoke");
        return 42;
    }
}
"#;

const REDIRECT_FIELD: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class RedirectFieldMixin {
    @Redirect(method = "read()I", at = @At(value = "FIELD", target = "Ldemo/Target;count:I"))
    private static int read(CallbackInfo info) {
        Probe.note("redirect", "read");
        return 77;
    }

    @Redirect(method = "writeThenRead(I)I", at = @At(value = "FIELD", target = "Ldemo/Target;sink:I"))
    private static void write(int argument, int written, CallbackInfo info) {
        Probe.noteInt("redirect", written);
    }
}
"#;

const REDIRECT_HEAD: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Redirect;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class RedirectHeadMixin {
    @Redirect(method = "whole(I)I", at = @At("HEAD"))
    private static int whole(int value) {
        Probe.noteInt("head", value);
        return value + 100;
    }
}
"#;

#[test]
fn redirect_at_invoke_replaces_the_call_and_uses_its_result() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load(
        "redirect-invoke",
        "RedirectInvokeMixin.java",
        REDIRECT_INVOKE,
    );
    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].at, "INVOKE");
    assert!(plans[0].redirect);
    assert_eq!(jar.mixin_anchor_report().redirects, 1);

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("call", "()I"), None, &[], &mut host)
        .unwrap();
    assert_eq!(report.target_result, Value::Int(42));
    assert_eq!(report.matched_anchor_sites, 1);
    assert_eq!(notes(&host), vec![("redirect".into(), "invoke".into())]);
    assert!(
        !host
            .invocations()
            .iter()
            .any(|invocation| invocation.method.name == "value"),
        "the original Probe.value call must not run"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn redirect_at_field_replaces_reads_and_writes() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("redirect-field", "RedirectFieldMixin.java", REDIRECT_FIELD);
    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 2, "unexpected plans: {plans:#?}");
    assert!(plans.iter().all(|plan| plan.at == "FIELD" && plan.redirect));

    let mut host = StdHost::new();
    let read = jar
        .execute_discovered_anchors(&target("read", "()I"), None, &[], &mut host)
        .unwrap();
    assert_eq!(read.target_result, Value::Int(77));
    assert_eq!(read.matched_anchor_sites, 1);
    assert_eq!(notes(&host), vec![("redirect".into(), "read".into())]);

    let mut host = StdHost::new();
    let write = jar
        .execute_discovered_anchors(
            &target("writeThenRead", "(I)I"),
            None,
            &[Value::Int(7)],
            &mut host,
        )
        .unwrap();
    assert_eq!(
        write.target_result,
        Value::Int(0),
        "the redirected write leaves sink at its default"
    );
    assert_eq!(write.matched_anchor_sites, 1);
    assert_eq!(notes(&host), vec![("redirect".into(), "7".into())]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn redirect_at_head_replaces_the_whole_body() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("redirect-head", "RedirectHeadMixin.java", REDIRECT_HEAD);
    let plans = jar.mixin_anchor_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].at, "HEAD");

    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_anchors(&target("whole", "(I)I"), None, &[Value::Int(1)], &mut host)
        .unwrap();
    assert_eq!(report.target_result, Value::Int(101));
    assert_eq!(report.matched_anchor_sites, 0);
    assert_eq!(notes(&host), vec![("head".into(), "1".into())]);
    assert!(
        !notes(&host).iter().any(|(marker, _)| marker == "body"),
        "the target body must not run"
    );

    let _ = fs::remove_dir_all(root);
}

const UNRESOLVED: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class UnresolvedMixin {
    // NEW redirects are not executed natively.
    @Redirect(method = "make()V", at = @At(value = "NEW", target = "Ldemo/Made;"))
    private static void redirectNew(CallbackInfo info) { Probe.note("bad", "new"); }

    // Malformed FIELD selector.
    @Inject(method = "read()I", at = @At(value = "FIELD", target = "Ldemo/Target;count:NotADescriptor"))
    private static void badField(CallbackInfo info) { Probe.note("bad", "field"); }

    // FIELD read redirect with a void handler: the shape cannot be bounded.
    @Redirect(method = "read()I", at = @At(value = "FIELD", target = "Ldemo/Target;count:I"))
    private static void badRedirectShape(CallbackInfo info) { Probe.note("bad", "shape"); }

    // INVOKE redirect without a call descriptor: its return type is unknown.
    @Redirect(method = "call()I", at = @At(value = "INVOKE", target = "Ldemo/Probe;value"))
    private static int badInvokeSelector(CallbackInfo info) { return 0; }
}
"#;

#[test]
fn unresolved_selectors_and_shapes_stay_report_only() {
    if !has_jdk() {
        eprintln!("skipping: no JDK");
        return;
    }
    let (jar, root) = load("unresolved", "UnresolvedMixin.java", UNRESOLVED);

    assert!(
        jar.mixin_anchor_plans().is_empty(),
        "unbounded anchors must not be executable plans"
    );
    let unresolved = jar.unresolved_anchor_plans();
    assert_eq!(unresolved.len(), 4, "unexpected reasons: {unresolved:#?}");
    let reasons = unresolved
        .iter()
        .map(|(_, reason)| reason.as_str())
        .collect::<Vec<_>>();
    assert!(
        reasons.iter().any(|reason| reason.contains("NEW")),
        "missing NEW redirect reason: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("not one of the bounded")),
        "missing malformed FIELD selector reason: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("does not fit a FIELD redirect")),
        "missing FIELD redirect shape reason: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("omits the call descriptor")),
        "missing INVOKE redirect descriptor reason: {reasons:#?}"
    );
    assert_eq!(jar.mixin_anchor_report().unresolved, 4);

    let mut host = StdHost::new();
    let error = jar
        .execute_discovered_anchors(&target("read", "()I"), None, &[], &mut host)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no executable native Mixin anchor plans"),
        "unexpected error: {error}"
    );

    let _ = fs::remove_dir_all(root);
}

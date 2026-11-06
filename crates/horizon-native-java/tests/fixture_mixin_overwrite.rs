//! Runtime-built Mixin fixtures for `@Overwrite`, `@ModifyArg` and
//! `@ModifyVariable`.
//!
//! The jar comes from `javac`, so discovery reads the real
//! `RuntimeVisibleAnnotations` encoding.  `demo/Probe` exists only at compile
//! time: leaving it out of the jar makes every `Probe` call cross the host
//! boundary, where `StdHost` records the arguments the woven body actually
//! passed.
//!
//! The tests cover one successful shape per injector, the order the callbacks
//! run in, and the precise diagnostics for the shapes this bounded runtime
//! refuses.

use horizon_native_java::{MethodRef, MixinExecutionReport, NativeJar, StdHost, Value};
use std::{collections::BTreeMap, fs, io::Write, path::Path, path::PathBuf, process::Command};
use zip::write::SimpleFileOptions;

/// The `javac` this fixture should use, or `None` when no JDK is installed.
fn javac() -> Option<PathBuf> {
    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    Command::new(&javac)
        .arg("-version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| javac)
}

fn files_under(root: &Path) -> Vec<PathBuf> {
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

fn build_jar(name: &str, sources: BTreeMap<&str, &str>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-overwrite-{name}-{}",
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
    let status = Command::new(javac().expect("build_jar is only called after javac()"))
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
    zip.write_all(br#"{"id":"mixin-overwrite-fixture","version":"1.0"}"#)
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

/// Every `demo/Probe` call the host recorded, as `(name, arguments)`.
fn probe_calls(host: &StdHost) -> Vec<(String, Vec<Value>)> {
    host.invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .map(|invocation| (invocation.method.name.clone(), invocation.args.clone()))
        .collect()
}

fn run(
    jar: &NativeJar,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> (MixinExecutionReport, StdHost) {
    let mut host = StdHost::new();
    let report = jar
        .execute_discovered_mixins(&target(method, descriptor), None, args, &mut host)
        .unwrap();
    (report, host)
}

fn text(value: &str) -> Value {
    Value::Ref(Some(value.into()))
}

fn annotations() -> BTreeMap<&'static str, &'static str> {
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
        "org/spongepowered/asm/mixin/Overwrite.java",
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Overwrite {}
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
    int ordinal() default -1;
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
    At at();
    boolean cancellable() default false;
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/ModifyArg.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface ModifyArg {
    String[] method();
    At at();
    int index() default 0;
    boolean cancellable() default false;
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/injection/ModifyVariable.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface ModifyVariable {
    String[] method();
    At at();
    int index() default -1;
    int ordinal() default -1;
    String[] name() default {};
    boolean argsOnly() default false;
    boolean print() default false;
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
    public static int add(int left, int right) { return 0; }
    public static void mark(String marker, int value) {}
}
"#,
    );
    sources
}

fn target_source() -> &'static str {
    r#"package demo;
public final class Target {
    public static int tick(int value) {
        Probe.mark("target-tick", value);
        return value;
    }
    public static int pick(int value) {
        Probe.mark("target-pick", value);
        return value;
    }
    public static int dup(int value) {
        Probe.mark("target-dup", value);
        return value;
    }
    public static int both(int value) {
        Probe.mark("target-both", value);
        return value;
    }
    public static int run(int seed) {
        int direct = Probe.add(seed, 5);
        int second = Probe.add(direct, 7);
        return second;
    }
    public static int full(int seed) {
        return Probe.add(seed, 5);
    }
    public static int twice(int seed) {
        Probe.add(seed, 1);
        Probe.add(seed, 2);
        return 0;
    }
    public static int nestedArg(int seed) {
        int first = Probe.add(seed, 1);
        int second = helperArg(seed);
        return first + second;
    }
    private static int helperArg(int seed) {
        return Probe.add(seed, 2);
    }
    public static int local(int seed) {
        int value = seed + 1;
        int copy = value + 1;
        return copy;
    }
    public static int nested(int seed) {
        int value = seed + 1;
        int copy = helper(seed);
        return value + copy;
    }
    private static int helper(int seed) {
        int inner = seed + 10;
        return inner;
    }
    public static int ordinalLocal(int seed) {
        int first = seed + 1;
        int second = seed + 2;
        return first + second;
    }
    public static int captured(int seed) {
        int value = seed + 1;
        return value;
    }
}
"#
}

const OVERWRITE_MIXIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class OverwriteMixin {
    @Overwrite
    public static int tick(int value) {
        Probe.mark("overwrite", value);
        return value * 10;
    }
}
"#;

const OVERWRITE_REJECTS: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class BadOverwriteMixin {
    @Overwrite
    public static int missing(int value) {
        return value;
    }

    @Overwrite
    public static int pick(int value, int extra) {
        return value + extra;
    }

    @Overwrite
    public static int both(int value) {
        return value * 3;
    }

    @Inject(method = "both(I)I", at = @At("HEAD"))
    private static void beforeBoth(int value) {
        Probe.mark("both-before", value);
    }
}
"#;

const DUPLICATE_OVERWRITE_ONE: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class DuplicateOverwriteOne {
    @Overwrite
    public static int dup(int value) {
        return value + 1;
    }
}
"#;

const DUPLICATE_OVERWRITE_TWO: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class DuplicateOverwriteTwo {
    @Overwrite
    public static int dup(int value) {
        return value + 2;
    }
}
"#;

const MODIFY_ARG_MIXIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyArg;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class ArgsMixin {
    @ModifyArg(method = "run(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I"),
               index = 0)
    private static int bump(int value) {
        Probe.mark("arg", value);
        return value + 1;
    }

    @ModifyArg(method = "full(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I"),
               index = 1)
    private static int fullShape(int left, int right) {
        return left * right;
    }

    @ModifyArg(method = "twice(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I", ordinal = 1),
               index = 0)
    private static int onlySecond(int value) {
        return value + 50;
    }

    @ModifyArg(method = "nestedArg(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I"),
               index = 0)
    private static int nestedCall(int value) {
        return value + 1;
    }
}
"#;

const MODIFY_ARG_REJECTS: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyArg;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class BadArgsMixin {
    @ModifyArg(method = "run(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I"),
               cancellable = true)
    private static int cancellable(int value) {
        return value;
    }

    @ModifyArg(method = "run(I)I", at = @At("HEAD"))
    private static int wrongAnchor(int value) {
        return value;
    }

    @ModifyArg(method = "run(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I"),
               index = 5)
    private static int outOfRange(int value) {
        return value;
    }

    @ModifyArg(method = "run(I)I",
               at = @At(value = "INVOKE", target = "Ldemo/Probe;add(II)I"),
               index = 0)
    private static void wrongReturn(int value) {
    }
}
"#;

const MODIFY_VARIABLE_MIXIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyVariable;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class VariableMixin {
    @ModifyVariable(method = "local(I)I", at = @At("STORE"), index = 1)
    private static int stored(int value) {
        Probe.mark("store", value);
        return value * 2;
    }

    @ModifyVariable(method = "local(I)I", at = @At("LOAD"), index = 2)
    private static int loaded(int value) {
        Probe.mark("load", value);
        return value + 100;
    }

    @ModifyVariable(method = "nested(I)I", at = @At("STORE"), index = 1)
    private static int nestedStore(int value) {
        Probe.mark("nested", value);
        return value * 2;
    }

    @ModifyVariable(method = "ordinalLocal(I)I", at = @At("STORE"), ordinal = 1)
    private static int ordinate(int value) {
        return value * 10;
    }

    @ModifyVariable(method = "captured(I)I", at = @At("STORE"), index = 1)
    private static int withTargetArgs(int value, int seed) {
        Probe.mark("captured", seed);
        return value + seed;
    }
}
"#;

const MODIFY_VARIABLE_REJECTS: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyVariable;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(targets = {"Ldemo/Target;"})
public abstract class BadVariableMixin {
    @ModifyVariable(method = "local(I)I", at = @At("STORE"), index = 1, ordinal = 0)
    private static int bothSelectors(int value) {
        return value;
    }

    @ModifyVariable(method = "local(I)I", at = @At("STORE"))
    private static int implicit(int value) {
        return value;
    }

    @ModifyVariable(method = "local(I)I", at = @At("INVOKE"), index = 1)
    private static int wrongAnchor(int value) {
        return value;
    }

    @ModifyVariable(method = "local(I)I", at = @At("STORE"), index = 1)
    private static String wrongReturn(int value) {
        return "";
    }

    @ModifyVariable(method = "local(I)I", at = @At("STORE"), index = 1)
    private static int withInfo(int value, CallbackInfo info) {
        return value;
    }

    @ModifyVariable(method = "local(I)I", at = @At("STORE"), name = "value")
    private static int byName(int value) {
        return value;
    }
}
"#;

fn fixture_sources(extra: &[(&'static str, &'static str)]) -> BTreeMap<&'static str, &'static str> {
    let mut sources = annotations();
    sources.insert("demo/Target.java", target_source());
    for (path, body) in extra {
        sources.insert(path, body);
    }
    sources
}

#[test]
fn overwrite_replaces_the_target_body_and_counts_in_the_report() {
    if javac().is_none() {
        eprintln!("skipping overwrite fixture: no JDK");
        return;
    }
    let (root, jar_path) = build_jar(
        "overwrite",
        fixture_sources(&[("demo/OverwriteMixin.java", OVERWRITE_MIXIN)]),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let plans = jar.mixin_overwrite_plans();
    assert_eq!(plans.len(), 1, "unexpected plans: {plans:#?}");
    assert_eq!(plans[0].target, target("tick", "(I)I"));
    assert_eq!(plans[0].callback.name, "tick");
    assert!(plans[0].at.is_empty());
    assert!(jar.unresolved_overwrite_plans().is_empty());

    let report = jar.mixin_discovery_report();
    assert_eq!(report.overwrites, 1);
    assert_eq!(report.unresolved_overwrites, 0);
    assert!(report.plans() >= 1);

    let (report, host) = run(&jar, "tick", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.target_result, Value::Int(30));
    assert_eq!(report.callback_results, vec![Value::Int(30)]);
    assert_eq!(report.callback_result, Value::Int(30));
    assert!(!report.cancelled);
    assert_eq!(report.matched_invoke_sites, 0);
    assert_eq!(report.matched_anchor_sites, 0);
    assert_eq!(
        probe_calls(&host),
        vec![("mark".into(), vec![text("overwrite"), Value::Int(3)])],
        "only the overwrite body runs; the original body never marks"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn overwrite_rejects_unknown_targets_mismatches_duplicates_and_coexistence() {
    if javac().is_none() {
        eprintln!("skipping overwrite rejection fixture: no JDK");
        return;
    }
    let (root, jar_path) = build_jar(
        "overwrite-rejects",
        fixture_sources(&[
            ("demo/BadOverwriteMixin.java", OVERWRITE_REJECTS),
            ("demo/DuplicateOverwriteOne.java", DUPLICATE_OVERWRITE_ONE),
            ("demo/DuplicateOverwriteTwo.java", DUPLICATE_OVERWRITE_TWO),
        ]),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    // `both` is a valid overwrite (the @Inject is a separate family), so it is
    // executable; `missing` and `pick` are rejected precisely, and the two
    // `dup` overwrites compete for one target.
    let plans = jar.mixin_overwrite_plans();
    assert_eq!(
        plans
            .iter()
            .map(|plan| (plan.target.name.as_str(), plan.target.descriptor.as_str()))
            .collect::<Vec<_>>(),
        vec![("both", "(I)I")]
    );
    let unresolved = jar.unresolved_overwrite_plans();
    let reasons = unresolved
        .iter()
        .map(|(plan, reason)| {
            (
                plan.target.name.clone(),
                plan.target.descriptor.clone(),
                reason.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        reasons
            .iter()
            .any(|(name, _, reason)| name == "missing"
                && reason.contains("has no method named missing")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons.iter().any(|(name, _, reason)| name == "pick"
            && reason.contains("does not match any target demo/Target#pick")),
        "unexpected reasons: {reasons:#?}"
    );
    let dup = reasons
        .iter()
        .filter(|(name, _, _)| name == "dup")
        .collect::<Vec<_>>();
    assert_eq!(dup.len(), 2, "both competing overwrites are reported");
    assert!(
        dup.iter()
            .all(|(_, _, reason)| reason.contains("exactly one")),
        "unexpected reasons: {reasons:#?}"
    );
    let report = jar.mixin_discovery_report();
    assert_eq!(report.overwrites, 1);
    assert_eq!(report.unresolved_overwrites, 4);

    // A valid overwrite plus an @Inject on the same target is refused at
    // execution rather than running one and silently dropping the other.
    let mut host = StdHost::new();
    let error = jar
        .execute_discovered_mixins(&target("both", "(I)I"), None, &[Value::Int(1)], &mut host)
        .unwrap_err();
    assert!(
        error.to_string().contains("cannot coexist"),
        "unexpected error: {error}"
    );
    assert!(
        probe_calls(&host).is_empty(),
        "no callback runs when the plans conflict: {:?}",
        probe_calls(&host)
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn modify_arg_replaces_one_argument_before_each_matching_call() {
    if javac().is_none() {
        eprintln!("skipping modify-arg fixture: no JDK");
        return;
    }
    let (root, jar_path) = build_jar(
        "modify-arg",
        fixture_sources(&[("demo/ArgsMixin.java", MODIFY_ARG_MIXIN)]),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let plans = jar.mixin_modify_arg_plans();
    assert_eq!(plans.len(), 4, "unexpected plans: {plans:#?}");
    assert!(jar.unresolved_modify_arg_plans().is_empty());
    let report = jar.mixin_discovery_report();
    assert_eq!(report.modify_args, 4);
    assert_eq!(report.unresolved_modify_args, 0);

    // The simple shape replaces argument 0 at both `add` calls, and each
    // callback sees the value the woven body pushed.
    let (report, host) = run(&jar, "run", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(4), Value::Int(1)]);
    assert_eq!(report.target_result, Value::Int(0));
    assert_eq!(report.matched_invoke_sites, 2);
    assert_eq!(
        probe_calls(&host),
        vec![
            ("mark".into(), vec![text("arg"), Value::Int(3)]),
            ("add".into(), vec![Value::Int(4), Value::Int(5)]),
            ("mark".into(), vec![text("arg"), Value::Int(0)]),
            ("add".into(), vec![Value::Int(1), Value::Int(7)]),
        ],
        "each callback runs immediately before its call and its result is the argument"
    );

    // The full-subject shape receives the invoked method's whole argument
    // list, so it can combine the modified argument with its siblings.
    let (report, host) = run(&jar, "full", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(15)]);
    assert_eq!(
        probe_calls(&host),
        vec![("add".into(), vec![Value::Int(3), Value::Int(15)])]
    );

    // `ordinal = 1` only modifies the second matching call site.
    let (report, host) = run(&jar, "twice", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(53)]);
    assert_eq!(report.matched_invoke_sites, 1);
    assert_eq!(
        probe_calls(&host),
        vec![
            ("add".into(), vec![Value::Int(3), Value::Int(1)]),
            ("add".into(), vec![Value::Int(53), Value::Int(2)]),
        ]
    );

    // The hook belongs to `nestedArg`'s frame: the helper calls the same
    // site, but its argument is untouched.
    let (report, host) = run(&jar, "nestedArg", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(4)]);
    assert_eq!(
        probe_calls(&host),
        vec![
            ("add".into(), vec![Value::Int(4), Value::Int(1)]),
            ("add".into(), vec![Value::Int(3), Value::Int(2)]),
        ],
        "a callee's call site is not the woven method's"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn modify_arg_rejects_cancellable_wrong_anchor_range_and_return_shapes() {
    if javac().is_none() {
        eprintln!("skipping modify-arg rejection fixture: no JDK");
        return;
    }
    let (root, jar_path) = build_jar(
        "modify-arg-rejects",
        fixture_sources(&[("demo/BadArgsMixin.java", MODIFY_ARG_REJECTS)]),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    assert!(jar.mixin_modify_arg_plans().is_empty());
    let reasons = jar
        .unresolved_modify_arg_plans()
        .into_iter()
        .map(|(_, reason)| reason)
        .collect::<Vec<_>>();
    assert_eq!(reasons.len(), 4, "unexpected reasons: {reasons:#?}");
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("is cancellable")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("native argument modification supports INVOKE")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("is outside demo/Probe#add(II)I's 2 argument(s)")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("returns V but argument 0 of the call is I")),
        "unexpected reasons: {reasons:#?}"
    );
    assert_eq!(jar.mixin_discovery_report().modify_args, 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn modify_variable_replaces_store_and_load_values_on_selected_slots() {
    if javac().is_none() {
        eprintln!("skipping modify-variable fixture: no JDK");
        return;
    }
    let (root, jar_path) = build_jar(
        "modify-variable",
        fixture_sources(&[("demo/VariableMixin.java", MODIFY_VARIABLE_MIXIN)]),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let plans = jar.mixin_modify_variable_plans();
    assert_eq!(plans.len(), 5, "unexpected plans: {plans:#?}");
    assert_eq!(
        plans
            .iter()
            .map(|plan| (plan.at.as_str(), plan.index, plan.ordinal))
            .collect::<Vec<_>>(),
        vec![
            ("STORE", Some(1), None),
            ("LOAD", Some(2), None),
            ("STORE", Some(1), None),
            ("STORE", Some(1), None),
            ("STORE", None, Some(1)),
        ]
    );
    let report = jar.mixin_discovery_report();
    assert_eq!(report.modify_variables, 5);
    assert_eq!(report.unresolved_modify_variables, 0);

    // `seed + 1` is 4, the STORE callback doubles it to 8, `copy` becomes 9,
    // and the LOAD callback turns the return value into 109.
    let (report, host) = run(&jar, "local", "(I)I", &[Value::Int(3)]);
    assert_eq!(
        report.callback_results,
        vec![Value::Int(8), Value::Int(109)]
    );
    assert_eq!(report.target_result, Value::Int(109));
    assert_eq!(report.matched_anchor_sites, 2);
    assert_eq!(
        probe_calls(&host),
        vec![
            ("mark".into(), vec![text("store"), Value::Int(4)]),
            ("mark".into(), vec![text("load"), Value::Int(9)]),
        ],
        "the store callback sees the pushed value and the load callback the stored one"
    );

    // The hooks belong to `nested`'s frame only: the helper it calls also has
    // an int in slot 1, and must not run the callback.
    let (report, host) = run(&jar, "nested", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(8)]);
    assert_eq!(report.target_result, Value::Int(21));
    assert_eq!(
        probe_calls(&host),
        vec![("mark".into(), vec![text("nested"), Value::Int(4)])],
        "a callee's local store is not the woven method's"
    );

    // `ordinal = 1` selects the second int store (`seed + 2`), not the first.
    let (report, host) = run(&jar, "ordinalLocal", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(50)]);
    assert_eq!(report.target_result, Value::Int(54));
    assert_eq!(report.matched_anchor_sites, 1);
    assert!(probe_calls(&host).is_empty());

    // A callback may append the target method's arguments after the value.
    let (report, host) = run(&jar, "captured", "(I)I", &[Value::Int(3)]);
    assert_eq!(report.callback_results, vec![Value::Int(7)]);
    assert_eq!(report.target_result, Value::Int(7));
    assert_eq!(
        probe_calls(&host),
        vec![("mark".into(), vec![text("captured"), Value::Int(3)])],
        "the callback receives the target's own argument after the value"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn modify_variable_rejects_ambiguous_implicit_and_unsupported_shapes() {
    if javac().is_none() {
        eprintln!("skipping modify-variable rejection fixture: no JDK");
        return;
    }
    let (root, jar_path) = build_jar(
        "modify-variable-rejects",
        fixture_sources(&[("demo/BadVariableMixin.java", MODIFY_VARIABLE_REJECTS)]),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    assert!(jar.mixin_modify_variable_plans().is_empty());
    let reasons = jar
        .unresolved_modify_variable_plans()
        .into_iter()
        .map(|(_, reason)| reason)
        .collect::<Vec<_>>();
    assert_eq!(reasons.len(), 6, "unexpected reasons: {reasons:#?}");
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("declares both index 1 and ordinal 0")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("has no index or ordinal")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("native local hooks support STORE and LOAD")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("returns Ljava/lang/String; but captures a I local")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("takes a trailing CallbackInfo")),
        "unexpected reasons: {reasons:#?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("selects a local by name")),
        "unexpected reasons: {reasons:#?}"
    );
    assert_eq!(jar.mixin_discovery_report().modify_variables, 0);

    let _ = fs::remove_dir_all(root);
}

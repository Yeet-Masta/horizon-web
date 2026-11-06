//! Runtime-built fixtures for the load-time Mixin weave.
//!
//! These drive the path the game's loader takes: `NativeJar::from_path`, then
//! `plan_load_time_weave`, then `with_woven_classes`, then an ordinary
//! `execute`. No JVM and no `--weave` jar is involved, so a callback that runs
//! here ran from the rewritten bytecode of its target.
//!
//! `demo/Probe` exists only at compile time: leaving it out of the jar makes
//! every `Probe.mark` call cross the host boundary, where `StdHost` records the
//! order and the arguments.

use horizon_native_java::{MixinSide, MixinSkipReason, NativeJar, StdHost, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use zip::write::SimpleFileOptions;

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
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

/// `@Mixin`, with the retention the stub declares. Real Mixin uses `CLASS`;
/// `RUNTIME` is what makes the discovery path reachable from a fixture.
fn mixin_stub(retention: &str) -> String {
    format!(
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.{retention})
@Target(ElementType.TYPE)
public @interface Mixin {{
    Class<?>[] value() default {{}};
    String[] targets() default {{}};
    int priority() default 1000;
}}
"#
    )
}

const AT: &str = r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface At {
    String value() default "";
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

const CALLBACK_INFO: &str = r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfo {
    public void cancel() {}
    public boolean isCancelled() { return false; }
}
"#;

const CALLBACK_INFO_RETURNABLE: &str = r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfoReturnable<T> extends CallbackInfo {
    public void setReturnValue(T value) {}
    public T getReturnValue() { return null; }
}
"#;

const PROBE: &str = r#"package demo;
public final class Probe {
    public static void mark(String marker, int value) {}
}
"#;

/// Everything every fixture jar shares: the Mixin stubs, which are compile-time
/// only in a real jar too, and `Probe`.
const COMPILE_ONLY: &[&str] = &[
    "org/spongepowered/asm/mixin/Mixin.class",
    "org/spongepowered/asm/mixin/injection/At.class",
    "org/spongepowered/asm/mixin/injection/Inject.class",
    "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.class",
    "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable.class",
    "demo/Probe.class",
];

/// A fixture mod: `demo.Main` is its `main` entrypoint, `configs` are declared
/// by `fabric.mod.json` and shipped at the jar root.
struct Fixture {
    root: PathBuf,
    jar: PathBuf,
}

impl Fixture {
    fn build(
        name: &str,
        retention: &str,
        sources: &[(&str, &str)],
        configs: &[(&str, &str)],
    ) -> Self {
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-mixin-woven-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        fs::create_dir_all(&classes).unwrap();
        let mixin = mixin_stub(retention);
        let shared = [
            ("org/spongepowered/asm/mixin/Mixin.java", mixin.as_str()),
            ("org/spongepowered/asm/mixin/injection/At.java", AT),
            ("org/spongepowered/asm/mixin/injection/Inject.java", INJECT),
            (
                "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.java",
                CALLBACK_INFO,
            ),
            (
                "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable.java",
                CALLBACK_INFO_RETURNABLE,
            ),
            ("demo/Probe.java", PROBE),
        ];
        let mut paths = Vec::new();
        for (relative, body) in shared.iter().chain(sources) {
            let path = src.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
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

        let declared = configs
            .iter()
            .map(|(path, _)| format!("\"{path}\""))
            .collect::<Vec<_>>()
            .join(",");
        let metadata = format!(
            r#"{{"id":"woven-{name}","version":"1.0","entrypoints":{{"main":["demo.Main"]}},"mixins":[{declared}]}}"#
        );
        let jar = root.join(format!("{name}.jar"));
        let options = SimpleFileOptions::default();
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
        zip.start_file("fabric.mod.json", options).unwrap();
        zip.write_all(metadata.as_bytes()).unwrap();
        for (path, body) in configs {
            zip.start_file(*path, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        for class in files_under(&classes) {
            let entry = class
                .strip_prefix(&classes)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if COMPILE_ONLY.contains(&entry.as_str()) {
                continue;
            }
            zip.start_file(entry, options).unwrap();
            zip.write_all(&fs::read(class).unwrap()).unwrap();
        }
        zip.finish().unwrap();
        Self { root, jar }
    }

    fn load(&self) -> NativeJar {
        NativeJar::from_path(&self.jar).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Run the jar's `main` entrypoint and return the `Probe.mark` calls in order.
fn run_main(jar: &NativeJar) -> Vec<(String, i32)> {
    let mut host = jar.seed_host(StdHost::new());
    jar.execute("demo.Main", None, "main", &mut host).unwrap();
    probe_marks(&host)
}

/// [`run_main`] with the synthetic bridge instances the game's loader seeds
/// before it executes a woven jar.
fn run_woven_main(
    jar: &NativeJar,
    weave: &horizon_native_java::LoadTimeMixinWeave,
) -> Vec<(String, i32)> {
    let mut host = jar
        .seed_host(StdHost::new())
        .with_mixin_bridge_instances(&weave.classes)
        .unwrap();
    jar.execute("demo.Main", None, "main", &mut host).unwrap();
    probe_marks(&host)
}

/// The `Probe.mark` calls a completed run recorded, in order.
fn probe_marks(host: &StdHost) -> Vec<(String, i32)> {
    host.invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == "demo/Probe")
        .map(|invocation| match invocation.args.as_slice() {
            [Value::Ref(Some(marker)), Value::Int(value)] => (marker.clone(), *value),
            other => panic!("unexpected Probe.mark arguments: {other:?}"),
        })
        .collect()
}

fn marks(calls: &[(&str, i32)]) -> Vec<(String, i32)> {
    calls
        .iter()
        .map(|(marker, value)| ((*marker).to_owned(), *value))
        .collect()
}

/// The jar the game would execute: planned and rebuilt for `side`.
fn woven(jar: NativeJar, side: MixinSide) -> (NativeJar, horizon_native_java::LoadTimeMixinWeave) {
    let weave = jar.plan_load_time_weave(side);
    let jar = jar.with_woven_classes(&weave.classes).unwrap();
    (jar, weave)
}

const MAIN_RUNS_TARGET: &str = r#"package demo;
public final class Main {
    public void onInitialize() {
        Target.run();
        Probe.mark("after", Counter.value);
    }
}
"#;

const COUNTER: &str = r#"package demo;
public final class Counter {
    public static int value;
}
"#;

#[test]
fn a_woven_head_callback_runs_when_its_target_runs() {
    let fixture = Fixture::build(
        "counter",
        "RUNTIME",
        &[
            ("demo/Main.java", MAIN_RUNS_TARGET),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", Counter.value); }
}
"#,
            ),
            (
                "demo/TargetMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class TargetMixin {
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void onRun() {
        Counter.value++;
        Probe.mark("callback", Counter.value);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["TargetMixin"]}"#,
        )],
    );

    // The jar alone never runs the mixin: this is what production did before.
    let plain = fixture.load();
    assert_eq!(
        run_main(&plain),
        marks(&[("target", 0), ("after", 0)]),
        "an unwoven jar must not reach the callback"
    );

    let (jar, weave) = woven(plain.clone(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1);
    assert_eq!(weave.skipped, [], "nothing should be left out");
    assert_eq!(
        run_main(&jar),
        marks(&[("callback", 1), ("target", 1), ("after", 1)]),
        "the callback runs first and its static write is visible to the target"
    );

    // The weave changes classes, not what the jar says about itself.
    assert_eq!(jar.mixin_injection_plans().len(), 1);
    assert_eq!(jar.mixin_injection_plans(), plain.mixin_injection_plans());
    assert_eq!(jar.fabric_id.as_deref(), Some("woven-counter"));
    assert_eq!(jar.fabric_entrypoints, plain.fabric_entrypoints);
    assert_eq!(
        jar.resource_names().collect::<Vec<_>>(),
        plain.resource_names().collect::<Vec<_>>()
    );
    assert_eq!(jar.mixin_configs(), plain.mixin_configs());
}

#[test]
fn an_instance_callback_is_woven_through_its_bridge_and_runs() {
    let fixture = Fixture::build(
        "instance",
        "RUNTIME",
        &[
            ("demo/Main.java", MAIN_RUNS_TARGET),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", Counter.value); }
}
"#,
            ),
            (
                "demo/InstanceMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class InstanceMixin {
    // Package-visible: the synthetic bridge sits in this package.
    @Inject(method = "run()V", at = @At("HEAD"))
    void onRun() {
        Counter.value++;
        Probe.mark("instance", Counter.value);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["InstanceMixin"]}"#,
        )],
    );

    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.skipped, [], "{:#?}", weave.skipped);
    assert_eq!(weave.woven.len(), 1);
    assert_eq!(weave.woven[0].callback.owner, "demo/InstanceMixin");
    let bridge = weave
        .classes
        .added
        .keys()
        .find(|name| name.contains("$$HorizonBridge$"))
        .expect("an instance callback needs its bridge class");
    assert!(
        bridge.starts_with("demo/InstanceMixin$$HorizonBridge$"),
        "{bridge}"
    );
    let seeds = horizon_native_java::mixin_bridge_instances(&weave.classes).unwrap();
    assert_eq!(
        seeds,
        [horizon_native_java::MixinBridgeInstance {
            field: format!("{bridge}#callback0Ldemo/InstanceMixin;"),
            owner: "demo/InstanceMixin".into(),
        }]
    );
    // The instance callback runs from the woven target through the bridge the
    // host seeded, before the target body, exactly like a static one.
    assert_eq!(
        run_woven_main(&jar, &weave),
        marks(&[("instance", 1), ("target", 1), ("after", 1)])
    );
}

/// The names are deliberately not alphabetical: discovery sorts by callback
/// name, so a fixture of `first`/`second` would pass by accident.
#[test]
fn head_callbacks_run_in_mixin_apply_order() {
    let fixture = Fixture::build(
        "order",
        "RUNTIME",
        &[
            ("demo/Main.java", MAIN_RUNS_TARGET),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", 0); }
}
"#,
            ),
            (
                "demo/ZetaMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class ZetaMixin {
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void zulu() { Probe.mark("zulu", 0); }
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void bravo() { Probe.mark("bravo", 0); }
}
"#,
            ),
            (
                "demo/AlphaMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class AlphaMixin {
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void alpha() { Probe.mark("alpha", 0); }
}
"#,
            ),
            (
                "demo/EarlyMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(value = Target.class, priority = 500)
public abstract class EarlyMixin {
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void early() { Probe.mark("early", 0); }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["ZetaMixin","AlphaMixin","EarlyMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Client);
    assert_eq!(weave.skipped, []);
    let expected = marks(&[
        // Priority beats list position, list position beats the class name, and
        // a class's own callbacks keep the order of its methods.
        ("early", 0),
        ("zulu", 0),
        ("bravo", 0),
        ("alpha", 0),
        ("target", 0),
        ("after", 0),
    ]);
    assert_eq!(run_main(&jar), expected);
    // The plans say the same thing the bytecode does.
    let names = jar
        .mixin_plans_in_apply_order()
        .into_iter()
        .map(|plan| plan.callback.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["early", "zulu", "bravo", "alpha"]);
    assert_eq!(
        weave
            .woven
            .iter()
            .map(|plan| plan.callback.name.as_str())
            .collect::<Vec<_>>(),
        names
    );
}

#[test]
fn a_mixed_jar_weaves_one_target_and_reports_the_other() {
    let fixture = Fixture::build(
        "mixed",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        Target.run();
        Other.run();
    }
}
"#,
            ),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", 0); }
}
"#,
            ),
            (
                "demo/Other.java",
                r#"package demo;
public final class Other {
    public static void run() { Probe.mark("other", 0); }
}
"#,
            ),
            (
                "demo/PlainMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class PlainMixin {
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void woven() { Probe.mark("woven", 0); }
}
"#,
            ),
            (
                "demo/CancelMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Other.class)
public abstract class CancelMixin {
    @Inject(method = "run()V", at = @At("HEAD"), cancellable = true)
    private static void cancels(CallbackInfo info) { info.cancel(); }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["PlainMixin","CancelMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(jar.mixin_injection_plans().len(), 2);
    assert_eq!(
        weave
            .woven
            .iter()
            .map(|plan| plan.callback.name.as_str())
            .collect::<Vec<_>>(),
        ["cancels", "woven"]
    );
    assert_eq!(weave.skipped, [], "both shapes are inside the subset");
    // The cancellable target's body never runs: its callback cancelled at HEAD,
    // and the woven `if (isCancelled()) return` took the early answer.
    assert_eq!(run_main(&jar), marks(&[("woven", 0), ("target", 0)]));
    assert_eq!(weave.classes.classes.len(), 2);
    assert!(weave.classes.classes.contains_key("demo/Target"));
    assert!(weave.classes.classes.contains_key("demo/Other"));
    assert!(
        weave
            .classes
            .added
            .contains_key("org/spongepowered/asm/mixin/injection/callback/CallbackInfo")
    );
}

const REASONS_MAIN: &str = r#"package demo;
public final class Main {
    public void onInitialize() {
        Target.good();
        Target.both();
    }
}
"#;

const REASONS_TARGET: &str = r#"package demo;
public final class Target {
    public Target() {}
    public static void good() { Probe.mark("good-target", 0); }
    public static void both() { Probe.mark("both-target", 0); }
    public static int number() { Probe.mark("number", 0); return 3; }
    public static void over(int value) {}
    public static void over(long value) {}
    public static void takesInfo() {}
    public static void instanceCallback() {}
    public static void sided() {}
}
"#;

const REASONS_MIXIN: &str = r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class ReasonsMixin {
    // A name with no descriptor, resolved because only one method answers it.
    @Inject(method = "good", at = @At("HEAD"))
    private static void good() { Probe.mark("good", 0); }
    @Inject(method = "number()I", at = @At("RETURN"))
    private static void onReturned() {}
    @Inject(method = "over", at = @At("HEAD"))
    private static void onOver() {}
    @Inject(method = "missing()V", at = @At("HEAD"))
    private static void onMissing() {}
    @Inject(method = "<init>()V", at = @At("HEAD"))
    private static void onConstructor() {}
    // The shape real mods use for a plain HEAD: a trailing CallbackInfo.
    @Inject(method = "takesInfo()V", at = @At("HEAD"))
    private static void onTakesInfo(CallbackInfo info) {}
    @Inject(method = "instanceCallback()V", at = @At("HEAD"))
    private void onInstance() {}
    // Two selectors naming one method apply once, not twice.
    @Inject(method = {"both", "both()V"}, at = @At("HEAD"))
    private static void onBoth() { Probe.mark("both", 0); }
}
"#;

fn single_inject(class: &str, mixin_target: &str, method: &str) -> String {
    format!(
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(targets = "{mixin_target}")
public abstract class {class} {{
    @Inject(method = "{method}", at = @At("HEAD"))
    private static void callback() {{}}
}}
"#
    )
}

/// Every label a jar in this fixture's shape earns, sorted, with its count.
fn skip_labels(weave: &horizon_native_java::LoadTimeMixinWeave) -> BTreeMap<&'static str, usize> {
    let mut labels = BTreeMap::new();
    for row in &weave.skipped {
        *labels.entry(row.reason.label()).or_insert(0) += 1;
    }
    labels
}

#[test]
fn every_shape_outside_the_subset_is_skipped_with_its_reason() {
    let outside = single_inject("OutsideMixin", "Lnet/minecraft/Elsewhere;", "tick()V");
    let client = single_inject("ClientMixin", "Ldemo/Target;", "sided()V");
    let unlisted = single_inject("UnlistedMixin", "Ldemo/Target;", "sided()V");
    let plugged = single_inject("PluggedMixin", "Ldemo/Target;", "sided()V");
    let fixture = Fixture::build(
        "reasons",
        "RUNTIME",
        &[
            ("demo/Main.java", REASONS_MAIN),
            ("demo/Target.java", REASONS_TARGET),
            ("demo/ReasonsMixin.java", REASONS_MIXIN),
            ("demo/OutsideMixin.java", &outside),
            ("demo/ClientMixin.java", &client),
            ("demo/UnlistedMixin.java", &unlisted),
            ("demo/PluggedMixin.java", &plugged),
            (
                "demo/MemberMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
@Mixin(Target.class)
public abstract class MemberMixin {
    private int merged;
}
"#,
            ),
            (
                "demo/Listed.java",
                "package demo;\npublic final class Listed {}\n",
            ),
        ],
        &[
            (
                "demo.mixins.json",
                r#"{"package":"demo","mixins":["ReasonsMixin","OutsideMixin","MemberMixin","Listed"],"client":["ClientMixin"]}"#,
            ),
            (
                "plugged.mixins.json",
                r#"{"package":"demo","plugin":"demo.Plugin","mixins":["PluggedMixin"]}"#,
            ),
        ],
    );

    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    let expected = BTreeMap::from([
        // The `over` overloads, and `both`'s second selector for a method the
        // first one already wove.
        ("ambiguous-target", 2),
        ("config-plugin", 1),
        ("constructor-target", 1),
        ("listed-without-plan", 1),
        ("not-in-config", 1),
        ("other-side", 1),
        ("target-missing", 1),
        ("target-outside-jar", 1),
        ("unsupported-family", 1),
        // The private instance callback (the weaver cannot bridge it) and a
        // value-returning RETURN whose callback takes no CallbackInfoReturnable
        // (the insert would have no place to put the result).
        ("weaver-rejected", 2),
    ]);
    assert_eq!(skip_labels(&weave), expected, "{:#?}", weave.skipped);
    let private = weave
        .skipped
        .iter()
        .find(|row| row.callback.name == "onInstance")
        .expect("the private instance callback is a weaver reason");
    assert_eq!(private.reason.label(), "weaver-rejected");
    assert!(
        private.to_string().contains("demo/ReasonsMixin#onInstance"),
        "{private}"
    );
    let value_return = weave
        .skipped
        .iter()
        .find(|row| row.callback.name == "onReturned")
        .expect("the value-returning RETURN is reported");
    assert!(
        value_return.to_string().contains("CallbackInfoReturnable"),
        "{value_return}"
    );

    // A rejected plan costs only itself: the same class's good plans still run.
    let woven_names = weave
        .woven
        .iter()
        .map(|plan| plan.callback.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(woven_names, ["good", "onTakesInfo", "onBoth"]);
    assert_eq!(weave.woven[0].target.descriptor, "()V", "resolved overload");
    assert_eq!(
        run_main(&jar),
        marks(&[
            ("good", 0),
            ("good-target", 0),
            ("both", 0),
            ("both-target", 0)
        ]),
        "each callback runs exactly once"
    );

    // The same class list on the other side applies the `client` mixin.
    let (_, client_weave) = woven(fixture.load(), MixinSide::Client);
    assert!(
        client_weave
            .woven
            .iter()
            .any(|plan| plan.callback.owner == "demo/ClientMixin"),
        "{:#?}",
        client_weave.woven
    );
    assert!(
        client_weave
            .skipped
            .iter()
            .all(|row| row.reason != MixinSkipReason::OtherSide),
        "{:#?}",
        client_weave.skipped
    );
}

/// Real `@Mixin` is `CLASS` retention, so javac writes it into
/// `RuntimeInvisibleAnnotations`. Discovery must still find it: the class is
/// woven exactly like a runtime-visible one, rather than reading as a mod whose
/// mixins silently never ran.
#[test]
fn a_class_retention_mixin_is_discovered_and_woven() {
    let fixture = Fixture::build(
        "retention",
        "CLASS",
        &[
            ("demo/Main.java", MAIN_RUNS_TARGET),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", 0); }
}
"#,
            ),
            (
                "demo/RealMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class RealMixin {
    @Inject(method = "run()V", at = @At("HEAD"))
    private static void onRun() { Probe.mark("never", 0); }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["RealMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(
        jar.mixin_injection_plans().len(),
        1,
        "{:?}",
        jar.mixin_injection_plans()
    );
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert_eq!(weave.woven[0].callback.owner, "demo/RealMixin");
    assert!(weave.skipped.is_empty(), "{:#?}", weave.skipped);
    assert_eq!(
        run_main(&jar),
        marks(&[("never", 0), ("target", 0), ("after", 0)])
    );
}

/// `HORIZON_REAL_MIXIN_JAR` names a real mod jar that lists Mixin classes. It
/// is opened read-only and never executed; without the variable the test skips,
/// because a real jar cannot live in this tree.
#[test]
fn a_real_mixin_jar_never_reads_as_fully_applied() {
    let Some(path) = std::env::var_os("HORIZON_REAL_MIXIN_JAR") else {
        println!("skip: HORIZON_REAL_MIXIN_JAR is not set");
        return;
    };
    let jar = NativeJar::from_path(Path::new(&path)).unwrap();
    let listed = jar
        .mixin_configs()
        .iter()
        .map(|config| config.mixins.len() + config.client.len() + config.server.len())
        .sum::<usize>();
    assert!(listed > 0, "{path:?} lists no Mixin classes");
    let weave = jar.plan_load_time_weave(MixinSide::Server);
    println!(
        "{path:?}: {listed} listed, {} plan(s), {} woven, {} skipped: {:?}",
        jar.mixin_injection_plans().len(),
        weave.woven.len(),
        weave.skipped.len(),
        skip_labels(&weave),
    );
    assert!(!weave.skipped.is_empty());
}

/// A `void` cancellable `HEAD` plan is woven, and its `CallbackInfo` is the
/// synthetic class the weaver adds: not cancelling keeps the body.
#[test]
fn a_woven_cancellable_head_that_does_not_cancel_runs_the_body() {
    let fixture = Fixture::build(
        "cancellable-keeps",
        "RUNTIME",
        &[
            ("demo/Main.java", MAIN_RUNS_TARGET),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Counter.value++; Probe.mark("target", Counter.value); }
}
"#,
            ),
            (
                "demo/KeepMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class KeepMixin {
    @Inject(method = "run()V", at = @At("HEAD"), cancellable = true)
    private static void keeps(CallbackInfo info) {
        Counter.value += 10;
        Probe.mark("callback", Counter.value);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["KeepMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert!(weave.skipped.is_empty(), "{:#?}", weave.skipped);
    assert_eq!(
        run_main(&jar),
        marks(&[("callback", 10), ("target", 11), ("after", 11)])
    );
}

/// The cancelling case: the target body never runs, so neither its mark nor its
/// write is visible after the call.
#[test]
fn a_woven_cancellable_head_that_cancels_skips_the_body() {
    let fixture = Fixture::build(
        "cancellable-cancels",
        "RUNTIME",
        &[
            ("demo/Main.java", MAIN_RUNS_TARGET),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Counter.value += 5; Probe.mark("target", Counter.value); }
}
"#,
            ),
            (
                "demo/CancelMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class CancelMixin {
    @Inject(method = "run()V", at = @At("HEAD"), cancellable = true)
    private static void cancels(CallbackInfo info) {
        info.cancel();
        Probe.mark("callback", 0);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["CancelMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert!(weave.skipped.is_empty(), "{:#?}", weave.skipped);
    assert_eq!(run_main(&jar), marks(&[("callback", 0), ("after", 0)]));
}

/// A woven target called in a loop constructs a `CallbackInfo` per call; the
/// woven shape keeps that object on the stack, not in the interpreter's
/// injection-point table, so the loop does not spend a bound.
#[test]
fn a_woven_cancellable_head_survives_a_hot_loop() {
    let fixture = Fixture::build(
        "cancellable-loop",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        for (int i = 0; i < 100; i++) {
            Target.run();
        }
        Probe.mark("after", Counter.value);
    }
}
"#,
            ),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Counter.value -= 1000; Probe.mark("target", Counter.value); }
}
"#,
            ),
            (
                "demo/CancelMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class CancelMixin {
    @Inject(method = "run()V", at = @At("HEAD"), cancellable = true)
    private static void cancels(CallbackInfo info) {
        Counter.value++;
        info.cancel();
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["CancelMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert_eq!(run_main(&jar), marks(&[("after", 100)]));
}

/// A non-void target would need a `CallbackInfoReturnable` carrying the
/// descriptor's default value; it stays out with its reason and runs untouched.
#[test]
fn a_cancellable_head_on_a_non_void_target_stays_out() {
    let fixture = Fixture::build(
        "cancellable-value",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        Probe.mark("after", Target.number());
    }
}
"#,
            ),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static int number() { Probe.mark("target", 0); return 7; }
}
"#,
            ),
            (
                "demo/ValueMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class ValueMixin {
    @Inject(method = "number()I", at = @At("HEAD"), cancellable = true)
    private static void cancels(CallbackInfo info) { info.cancel(); }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["ValueMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert!(weave.woven.is_empty(), "{:#?}", weave.woven);
    assert_eq!(weave.skipped.len(), 1);
    assert_eq!(weave.skipped[0].reason, MixinSkipReason::Cancellable);
    assert_eq!(
        run_main(&jar),
        marks(&[("target", 0), ("after", 7)]),
        "the unwoven target keeps its own answer"
    );
}

/// A `void` `RETURN` plan is woven: the callback runs after the body, at every
/// return instruction.
#[test]
fn a_woven_return_callback_runs_after_the_body() {
    let fixture = Fixture::build(
        "return-void",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        Target.run();
        Target.branchy();
        Target.branchy();
        Probe.mark("after", Counter.value);
    }
}
"#,
            ),
            ("demo/Counter.java", COUNTER),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", Counter.value); }
    public static void branchy() { if (Counter.value > 0) { return; } Counter.value++; }
}
"#,
            ),
            (
                "demo/ReturnMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class ReturnMixin {
    @Inject(method = "run()V", at = @At("RETURN"))
    private static void onRun() { Probe.mark("return", Counter.value); }
    @Inject(method = "branchy()V", at = @At("TAIL"))
    private static void onBranchy() { Probe.mark("tail", Counter.value); }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["ReturnMixin"]}"#,
        )],
    );
    let plain = fixture.load();
    assert_eq!(
        run_main(&plain),
        marks(&[("target", 0), ("after", 1)]),
        "an unwoven jar never reaches the callbacks"
    );
    let (jar, weave) = woven(plain, MixinSide::Server);
    assert_eq!(
        weave
            .woven
            .iter()
            .map(|plan| plan.callback.name.as_str())
            .collect::<Vec<_>>(),
        ["onRun", "onBranchy"]
    );
    assert!(weave.skipped.is_empty(), "{:#?}", weave.skipped);
    // `run` returns once. A `TAIL` insert sits on the method's last return, so
    // only `branchy`'s fall-through path reaches it; its early return does not.
    assert_eq!(
        run_main(&jar),
        marks(&[("target", 0), ("return", 0), ("tail", 1), ("after", 1)])
    );
}

/// A `void` `RETURN` whose callback expects a `CallbackInfo` gets one, and
/// cancelling it is meaningless at a return site (the body already ran).
#[test]
fn a_woven_return_callback_can_take_a_callback_info() {
    let fixture = Fixture::build(
        "return-info",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        Target.run();
    }
}
"#,
            ),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static void run() { Probe.mark("target", 0); }
}
"#,
            ),
            (
                "demo/InfoMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class InfoMixin {
    @Inject(method = "run()V", at = @At("RETURN"))
    private static void onRun(CallbackInfo info) {
        Probe.mark("return", info.isCancelled() ? 1 : 0);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["InfoMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert!(weave.skipped.is_empty(), "{:#?}", weave.skipped);
    assert_eq!(run_main(&jar), marks(&[("target", 0), ("return", 0)]));
}

/// A value-returning `RETURN` insert carries the result in a
/// `CallbackInfoReturnable` the weave builds; an override reaches the caller.
#[test]
fn a_woven_return_can_replace_a_value_targets_result() {
    let fixture = Fixture::build(
        "return-value-override",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        Probe.mark("after", Target.number());
    }
}
"#,
            ),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static int number() { Probe.mark("target", 0); return 3; }
}
"#,
            ),
            (
                "demo/ValueMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
@Mixin(Target.class)
public abstract class ValueMixin {
    @Inject(method = "number()I", at = @At("RETURN"))
    private static void onNumber(CallbackInfoReturnable<Integer> info) {
        Probe.mark("return", 0);
        info.setReturnValue(9);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["ValueMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert!(weave.skipped.is_empty(), "{:#?}", weave.skipped);
    assert!(
        weave
            .classes
            .added
            .contains_key("org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable"),
        "{:#?}",
        weave.classes.added.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        run_main(&jar),
        marks(&[("target", 0), ("return", 0), ("after", 9)])
    );
}

/// Without an override, the insert answers the target's own result.
#[test]
fn a_woven_return_keeps_the_targets_value_when_it_sets_none() {
    let fixture = Fixture::build(
        "return-value-plain",
        "RUNTIME",
        &[
            (
                "demo/Main.java",
                r#"package demo;
public final class Main {
    public void onInitialize() {
        Probe.mark("after", Target.number());
    }
}
"#,
            ),
            (
                "demo/Target.java",
                r#"package demo;
public final class Target {
    public static int number() { Probe.mark("target", 0); return 3; }
}
"#,
            ),
            (
                "demo/ValueMixin.java",
                r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;
@Mixin(Target.class)
public abstract class ValueMixin {
    @Inject(method = "number()I", at = @At("RETURN"))
    private static void onNumber(CallbackInfoReturnable<Integer> info) {
        Probe.mark("return", info.getReturnValue() == null ? 1 : 0);
    }
}
"#,
            ),
        ],
        &[(
            "demo.mixins.json",
            r#"{"package":"demo","mixins":["ValueMixin"]}"#,
        )],
    );
    let (jar, weave) = woven(fixture.load(), MixinSide::Server);
    assert_eq!(weave.woven.len(), 1, "{:#?}", weave.woven);
    assert_eq!(
        run_main(&jar),
        marks(&[("target", 0), ("return", 0), ("after", 3)])
    );
}

//! The Mixin family matrix: every injection family through the load-time path.
//!
//! One jar declares one mixin for each family (`HEAD`, cancellable `HEAD`,
//! `RETURN`, `TAIL`, `INVOKE`, `FIELD`, `NEW`, `JUMP`, `@Overwrite`,
//! `@ModifyArg`, `@ModifyVariable`).  Each family targets its own method of
//! `demo/Target` and writes a marker with that method's argument, so a family
//! that is applied, skipped or garbled cannot hide behind another one.
//!
//! The tests drive the same path the game's loader takes:
//! `NativeJar::from_path`, `plan_load_time_weave`, `with_woven_classes`, then an
//! ordinary `execute` of the jar's `main` entrypoint.  They then read
//! `LoadTimeMixinWeave::woven`/`skipped` and the `Probe.mark` calls the run
//! recorded at the host boundary (`demo/Probe` stays out of the jar).
//!
//! ## Expected failures are assertions, not skips
//!
//! The load-time weave applies `HEAD`, cancellable `HEAD` (on `void`),
//! `RETURN` and `TAIL`.  The other seven families are discovered but not
//! applied: `INVOKE` is a `non-head-anchor` skip, and `FIELD`, `NEW`, `JUMP`,
//! `@Overwrite`, `@ModifyArg` and `@ModifyVariable` are `unsupported-family`
//! skips.  [`unsupported_families_are_skipped_with_their_exact_reason`] asserts
//! that exact row, the callback marker's absence from the run, and the
//! target's own unchanged marker.  If a family starts (or stops) working, the
//! matrix fails until its row is updated: the gap is visible in CI rather than
//! silently skipped.

use horizon_native_java::{LoadTimeMixinWeave, MixinSide, NativeJar, StdHost, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};
use zip::write::SimpleFileOptions;

// ---------------------------------------------------------------------------
// The families under test
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Head,
    Cancellable,
    Return,
    Tail,
    Invoke,
    Field,
    New,
    Jump,
    Overwrite,
    ModifyArg,
    ModifyVariable,
}

impl Family {
    const ALL: [Family; 11] = [
        Family::Head,
        Family::Cancellable,
        Family::Return,
        Family::Tail,
        Family::Invoke,
        Family::Field,
        Family::New,
        Family::Jump,
        Family::Overwrite,
        Family::ModifyArg,
        Family::ModifyVariable,
    ];

    fn name(self) -> &'static str {
        match self {
            Family::Head => "head",
            Family::Cancellable => "cancellable",
            Family::Return => "return",
            Family::Tail => "tail",
            Family::Invoke => "invoke",
            Family::Field => "field",
            Family::New => "new",
            Family::Jump => "jump",
            Family::Overwrite => "overwrite",
            Family::ModifyArg => "modifyarg",
            Family::ModifyVariable => "modifyvar",
        }
    }

    /// The internal name of the one mixin class that declares this family.
    fn mixin_class(self) -> &'static str {
        match self {
            Family::Head => "demo/HeadMixin",
            Family::Cancellable => "demo/CancellableMixin",
            Family::Return => "demo/ReturnMixin",
            Family::Tail => "demo/TailMixin",
            Family::Invoke => "demo/InvokeMixin",
            Family::Field => "demo/FieldMixin",
            Family::New => "demo/NewMixin",
            Family::Jump => "demo/JumpMixin",
            Family::Overwrite => "demo/OverwriteMixin",
            Family::ModifyArg => "demo/ModifyArgMixin",
            Family::ModifyVariable => "demo/ModifyVariableMixin",
        }
    }

    /// The mixin callback method inside [`Self::mixin_class`].
    fn callback(self) -> &'static str {
        match self {
            Family::Head => "onHead",
            Family::Cancellable => "onCancel",
            Family::Return => "onReturn",
            Family::Tail => "onTail",
            Family::Invoke => "onInvoke",
            Family::Field => "onField",
            Family::New => "onNew",
            Family::Jump => "onJump",
            Family::Overwrite => "overwritten",
            Family::ModifyArg => "modifyArg",
            Family::ModifyVariable => "modifyVar",
        }
    }

    /// The marker the family's callback writes when it runs.
    fn callback_marker(self) -> &'static str {
        match self {
            Family::Head => "head-callback",
            Family::Cancellable => "cancel-callback",
            Family::Return => "return-callback",
            Family::Tail => "tail-callback",
            Family::Invoke => "invoke-callback",
            Family::Field => "field-callback",
            Family::New => "new-callback",
            Family::Jump => "jump-callback",
            Family::Overwrite => "overwrite-callback",
            Family::ModifyArg => "modifyarg-callback",
            Family::ModifyVariable => "modifyvar-callback",
        }
    }

    /// The marker the target writes on its unmodified path.
    fn target_marker(self) -> &'static str {
        match self {
            Family::Head => "head-target",
            Family::Cancellable => "cancelled-target",
            Family::Return => "returned-target",
            Family::Tail => "tailed-target",
            Family::Invoke => "invoked-target",
            Family::Field => "fielded-target",
            Family::New => "created-target",
            Family::Jump => "jumped-target",
            Family::Overwrite => "overwritten-target",
            Family::ModifyArg => "arg-call",
            Family::ModifyVariable => "var-call",
        }
    }

    /// The argument `Main` passes to this family's target method.
    fn argument(self) -> i32 {
        match self {
            Family::Head => 1,
            Family::Cancellable => 2,
            Family::Return => 3,
            Family::Tail => 4,
            Family::Invoke => 5,
            Family::Field => 6,
            Family::New => 7,
            Family::Jump => 8,
            Family::Overwrite => 9,
            Family::ModifyArg => 10,
            Family::ModifyVariable => 11,
        }
    }

    fn supported(self) -> bool {
        matches!(
            self,
            Family::Head | Family::Cancellable | Family::Return | Family::Tail
        )
    }

    /// The `@At` label the skip row carries for this family.  The family
    /// annotation, not an `@At`, is the label for the last three.
    fn skip_at(self) -> &'static str {
        match self {
            Family::Invoke => "INVOKE",
            Family::Field => "FIELD",
            Family::New => "NEW",
            Family::Jump => "JUMP",
            Family::Overwrite => "@Overwrite",
            Family::ModifyArg => "@ModifyArg",
            Family::ModifyVariable => "@ModifyVariable",
            Family::Head | Family::Cancellable | Family::Return | Family::Tail => {
                unreachable!("supported family has no skip row")
            }
        }
    }

    /// The fixed `MixinSkipReason` label for the unsupported family.
    fn skip_reason(self) -> &'static str {
        match self {
            Family::Invoke => "non-head-anchor",
            Family::Field
            | Family::New
            | Family::Jump
            | Family::Overwrite
            | Family::ModifyArg
            | Family::ModifyVariable => "unsupported-family",
            Family::Head | Family::Cancellable | Family::Return | Family::Tail => {
                unreachable!("supported family has no skip row")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture jar
// ---------------------------------------------------------------------------

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

/// The annotation stubs real Mixin ships.  `RUNTIME` retention is what makes
/// the discovery path reachable from a fixture; real Mixin uses `CLASS` and
/// reads the same annotations through its own class metadata.
const STUBS: &[(&str, &str)] = &[
    (
        "org/spongepowered/asm/mixin/Mixin.java",
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface Mixin {
    Class<?>[] value() default {};
    String[] targets() default {};
    int priority() default 1000;
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/Overwrite.java",
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Overwrite {}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/At.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target({})
public @interface At {
    String value() default "";
    String target() default "";
    int opcode() default -1;
    int ordinal() default -1;
}
"#,
    ),
    (
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
    ),
    (
        "org/spongepowered/asm/mixin/injection/ModifyArg.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface ModifyArg {
    String[] method();
    At[] at();
    int index() default 0;
    boolean cancellable() default false;
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/ModifyVariable.java",
        r#"package org.spongepowered.asm.mixin.injection;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface ModifyVariable {
    String[] method();
    At[] at();
    int index() default -1;
    int ordinal() default -1;
    String name() default "";
    boolean argsOnly() default false;
    boolean print() default false;
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.java",
        r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfo {
    public void cancel() {}
    public boolean isCancelled() { return false; }
}
"#,
    ),
    (
        "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable.java",
        r#"package org.spongepowered.asm.mixin.injection.callback;
public class CallbackInfoReturnable<T> extends CallbackInfo {
    public void setReturnValue(T value) {}
    public T getReturnValue() { return null; }
}
"#,
    ),
    (
        "demo/Probe.java",
        r#"package demo;
public final class Probe {
    public static void mark(String marker, int value) {}
}
"#,
    ),
];

/// Stubs plus `Probe` are compile-time only in this fixture: the jar must hold
/// the mod classes, and every `Probe.mark` call must cross the host boundary.
const COMPILE_ONLY: &[&str] = &[
    "org/spongepowered/asm/mixin/Mixin.class",
    "org/spongepowered/asm/mixin/Overwrite.class",
    "org/spongepowered/asm/mixin/injection/At.class",
    "org/spongepowered/asm/mixin/injection/Inject.class",
    "org/spongepowered/asm/mixin/injection/ModifyArg.class",
    "org/spongepowered/asm/mixin/injection/ModifyVariable.class",
    "org/spongepowered/asm/mixin/injection/callback/CallbackInfo.class",
    "org/spongepowered/asm/mixin/injection/callback/CallbackInfoReturnable.class",
    "demo/Probe.class",
];

/// `Main` runs every family's target in matrix order.  `begin`/`end` bracket
/// the run so a call that never happens is still visible.
const MAIN: &str = r#"package demo;
public final class Main {
    public void onInitialize() {
        Probe.mark("begin", 0);
        Target.head(1);
        Target.cancelled(2);
        Probe.mark("cancel-returned", 0);
        Target.returned(3);
        Target.tailed(4);
        Target.invoked(5);
        Target.fielded(6);
        Target.created(7);
        Target.jumped(8);
        Target.overwritten(9);
        Target.arg(10);
        Target.var(11);
        Probe.mark("end", 0);
    }
}
"#;

/// One method per family.  Each writes its own target marker so the matrix can
/// tell "not applied" from "applied to the wrong target".
const TARGET: &str = r#"package demo;
public final class Target {
    public static int field = 5;

    public static void head(int value) { Probe.mark("head-target", value); }

    public static void cancelled(int value) { Probe.mark("cancelled-target", value); }

    public static void returned(int value) { Probe.mark("returned-target", value); }

    public static void tailed(int value) { Probe.mark("tailed-target", value); }

    public static void invoked(int value) {
        Probe.mark("invoked-target", value);
        Probe.mark("invoke-call", value);
    }

    public static void fielded(int value) {
        int read = field;
        field = read + value;
        Probe.mark("fielded-target", field);
    }

    public static void created(int value) {
        Object created = new Object();
        Probe.mark("created-target", created == null ? -1 : value);
    }

    public static void jumped(int value) {
        if (value > 0) {
            Probe.mark("jumped-target", value);
            return;
        }
        Probe.mark("jumped-else", value);
    }

    public static void overwritten(int value) { Probe.mark("overwritten-target", value); }

    public static void arg(int value) { Probe.mark("arg-call", value); }

    public static void var(int value) {
        int local = value + 1;
        Probe.mark("var-call", local);
    }
}
"#;

/// The eleven mixins, one family each.  Every callback is `static` so the
/// weave does not need a synthetic bridge instance.
const MIXINS: &[(&str, &str)] = &[
    (
        "demo/HeadMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class HeadMixin {
    @Inject(method = "head(I)V", at = @At("HEAD"))
    private static void onHead(int value) { Probe.mark("head-callback", value); }
}
"#,
    ),
    (
        "demo/CancellableMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class CancellableMixin {
    @Inject(method = "cancelled(I)V", at = @At("HEAD"), cancellable = true)
    private static void onCancel(int value, CallbackInfo info) {
        Probe.mark("cancel-callback", value);
        info.cancel();
    }
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
    @Inject(method = "returned(I)V", at = @At("RETURN"))
    private static void onReturn(int value) { Probe.mark("return-callback", value); }
}
"#,
    ),
    (
        "demo/TailMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class TailMixin {
    @Inject(method = "tailed(I)V", at = @At("TAIL"))
    private static void onTail(int value) { Probe.mark("tail-callback", value); }
}
"#,
    ),
    (
        "demo/InvokeMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
@Mixin(Target.class)
public abstract class InvokeMixin {
    @Inject(method = "invoked(I)V",
            at = @At(value = "INVOKE", target = "Ldemo/Probe;mark(Ljava/lang/String;I)V"))
    private static void onInvoke(int value, CallbackInfo info) {
        Probe.mark("invoke-callback", value);
    }
}
"#,
    ),
    (
        "demo/FieldMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class FieldMixin {
    @Inject(method = "fielded(I)V",
            at = @At(value = "FIELD", target = "Ldemo/Target;field:I"))
    private static void onField(int value) { Probe.mark("field-callback", value); }
}
"#,
    ),
    (
        "demo/NewMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class NewMixin {
    @Inject(method = "created(I)V",
            at = @At(value = "NEW", target = "Ljava/lang/Object;"))
    private static void onNew(int value) { Probe.mark("new-callback", value); }
}
"#,
    ),
    (
        "demo/JumpMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
@Mixin(Target.class)
public abstract class JumpMixin {
    @Inject(method = "jumped(I)V", at = @At(value = "JUMP", opcode = 153))
    private static void onJump(int value) { Probe.mark("jump-callback", value); }
}
"#,
    ),
    (
        "demo/OverwriteMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Overwrite;
@Mixin(Target.class)
public abstract class OverwriteMixin {
    @Overwrite
    public static void overwritten(int value) { Probe.mark("overwrite-callback", value); }
}
"#,
    ),
    (
        "demo/ModifyArgMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyArg;
@Mixin(Target.class)
public abstract class ModifyArgMixin {
    @ModifyArg(method = "arg(I)V",
            at = @At(value = "INVOKE", target = "Ldemo/Probe;mark(Ljava/lang/String;I)V"),
            index = 1)
    private static int modifyArg(int value) {
        Probe.mark("modifyarg-callback", value);
        return value + 1;
    }
}
"#,
    ),
    (
        "demo/ModifyVariableMixin.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.ModifyVariable;
@Mixin(Target.class)
public abstract class ModifyVariableMixin {
    @ModifyVariable(method = "var(I)V", at = @At("STORE"), ordinal = 0)
    private static int modifyVar(int value) {
        Probe.mark("modifyvar-callback", value);
        return value + 2;
    }
}
"#,
    ),
];

const CONFIG: &str = r#"{"package":"demo","mixins":["HeadMixin","CancellableMixin","ReturnMixin","TailMixin","InvokeMixin","FieldMixin","NewMixin","JumpMixin","OverwriteMixin","ModifyArgMixin","ModifyVariableMixin"]}"#;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn build() -> Self {
        let root = std::env::temp_dir().join(format!(
            "horizon-native-java-mixin-family-matrix-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let classes = root.join("classes");
        fs::create_dir_all(&classes).unwrap();

        let mut paths = Vec::new();
        for (relative, body) in STUBS.iter().chain(MIXINS) {
            let path = src.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            paths.push(path);
        }
        for (relative, body) in [("demo/Main.java", MAIN), ("demo/Target.java", TARGET)] {
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
        assert!(status.success(), "javac failed for the mixin family matrix");

        let jar_path = root.join("family-matrix.jar");
        let options = SimpleFileOptions::default();
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        zip.start_file("fabric.mod.json", options).unwrap();
        zip.write_all(
            br#"{"id":"mixin-family-matrix","version":"1.0","entrypoints":{"main":["demo.Main"]},"mixins":["demo.mixins.json"]}"#,
        )
        .unwrap();
        zip.start_file("demo.mixins.json", options).unwrap();
        zip.write_all(CONFIG.as_bytes()).unwrap();
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
        Self { root }
    }

    fn jar(&self) -> PathBuf {
        self.root.join("family-matrix.jar")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// One `LoadTimeMixinWeave::skipped` row, flattened so the report can outlive
/// the weave.
#[derive(Debug)]
struct SkipRow {
    owner: String,
    callback: String,
    at: String,
    reason: String,
}

/// Everything the fixture run proved, captured once per test process.
struct MatrixReport {
    plain_marks: Vec<(String, i32)>,
    woven_marks: Vec<(String, i32)>,
    woven_callbacks: BTreeSet<String>,
    rows: Vec<SkipRow>,
    woven_plan_count: usize,
}

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

fn run(jar: &NativeJar, weave: Option<&LoadTimeMixinWeave>) -> Vec<(String, i32)> {
    let mut host = jar.seed_host(StdHost::new());
    if let Some(weave) = weave {
        host = host.with_mixin_bridge_instances(&weave.classes).unwrap();
    }
    jar.execute("demo.Main", None, "main", &mut host).unwrap();
    probe_marks(&host)
}

fn marks(calls: &[(&str, i32)]) -> Vec<(String, i32)> {
    calls
        .iter()
        .map(|(marker, value)| ((*marker).to_owned(), *value))
        .collect()
}

fn build_report() -> MatrixReport {
    let fixture = Fixture::build();
    let plain = NativeJar::from_path(&fixture.jar()).unwrap();
    let plain_marks = run(&plain, None);

    let weave = plain.plan_load_time_weave(MixinSide::Server);
    let woven = plain
        .clone()
        .with_woven_classes(&weave.classes)
        .expect("the load-time weave must produce parseable classes");
    let woven_marks = run(&woven, Some(&weave));

    let woven_callbacks = weave
        .woven
        .iter()
        .map(|plan| plan.callback.owner.clone())
        .collect::<BTreeSet<_>>();
    let rows = weave
        .skipped
        .iter()
        .map(|row| SkipRow {
            owner: row.callback.owner.clone(),
            callback: row.callback.name.clone(),
            at: row.at.clone(),
            reason: row.reason.label().to_owned(),
        })
        .collect::<Vec<_>>();

    MatrixReport {
        plain_marks,
        woven_marks,
        woven_callbacks,
        rows,
        woven_plan_count: weave.woven.len(),
    }
}

static REPORT: LazyLock<MatrixReport> = LazyLock::new(build_report);

fn report() -> &'static MatrixReport {
    &REPORT
}

impl MatrixReport {
    fn family_rows(&self, family: Family) -> Vec<&SkipRow> {
        self.rows
            .iter()
            .filter(|row| row.owner == family.mixin_class())
            .collect()
    }

    fn family_is_woven(&self, family: Family) -> bool {
        self.woven_callbacks.contains(family.mixin_class())
    }

    /// A fixed-width table for failure messages and `--nocapture` runs:
    /// `family pass/fail detail`.  A supported family passes when it weaves and
    /// runs; an unsupported one records the runtime's exact skip as a
    /// `fail (expected)` row, and the suite fails if that row moves.
    fn render(&self) -> String {
        let mut out = String::from("family       result         detail\n");
        let (mut passed, mut expected_failures) = (0, 0);
        for family in Family::ALL {
            let (result, detail) = if family.supported() {
                if self.family_is_woven(family) {
                    passed += 1;
                    (
                        "pass",
                        format!("{}#{} ran", family.mixin_class(), family.callback()),
                    )
                } else {
                    ("FAIL", "supported family was not woven".to_owned())
                }
            } else {
                let rows = self.family_rows(family);
                match rows.as_slice() {
                    [row] => {
                        expected_failures += 1;
                        ("fail (expected)", format!("{} at {}", row.reason, row.at))
                    }
                    [] => ("FAIL", "expected skip row is missing".to_owned()),
                    _ => ("FAIL", format!("{} skip rows, expected 1", rows.len())),
                }
            };
            out.push_str(&format!(
                "{:<12} {:<14} {}\n",
                family.name(),
                result,
                detail
            ));
        }
        out.push_str(&format!(
            "{passed} pass, {expected_failures} expected failure(s)\n"
        ));
        out
    }
}

/// The exact `Probe.mark` calls an unwoven run records: every target body, no
/// callback.  `fielded` adds `argument()` (6) to the initial field value 5.
#[test]
fn the_unwoven_jar_runs_each_target_but_no_callback() {
    assert_eq!(
        report().plain_marks,
        marks(&[
            ("begin", 0),
            ("head-target", 1),
            ("cancelled-target", 2),
            ("cancel-returned", 0),
            ("returned-target", 3),
            ("tailed-target", 4),
            ("invoked-target", 5),
            ("invoke-call", 5),
            ("fielded-target", 11),
            ("created-target", 7),
            ("jumped-target", 8),
            ("overwritten-target", 9),
            ("arg-call", 10),
            ("var-call", 12),
            ("end", 0),
        ]),
        "an unwoven jar must not reach any callback:\n{}",
        report().render()
    );
}

/// `HEAD`, cancellable `HEAD`, `RETURN` and `TAIL` weave and run with their own
/// argument, in their own position.  The cancellable callback suppresses its
/// target body; every other target body runs once.
#[test]
fn supported_families_weave_and_execute_with_independent_values() {
    let report = report();

    assert_eq!(
        report.woven_plan_count,
        4,
        "exactly the four executable families weave:\n{}",
        report.render()
    );
    assert_eq!(
        report.woven_callbacks.iter().cloned().collect::<Vec<_>>(),
        vec![
            "demo/CancellableMixin".to_owned(),
            "demo/HeadMixin".to_owned(),
            "demo/ReturnMixin".to_owned(),
            "demo/TailMixin".to_owned(),
        ],
        "unexpected woven callbacks:\n{}",
        report.render()
    );

    assert_eq!(
        report.woven_marks,
        marks(&[
            ("begin", 0),
            // HEAD: callback before the body.
            ("head-callback", 1),
            ("head-target", 1),
            // Cancellable HEAD: callback cancels, so the body never runs.
            ("cancel-callback", 2),
            ("cancel-returned", 0),
            // RETURN: body then callback, with the same argument.
            ("returned-target", 3),
            ("return-callback", 3),
            // TAIL: body then callback.
            ("tailed-target", 4),
            ("tail-callback", 4),
            // Unsupported families below run their targets unchanged.
            ("invoked-target", 5),
            ("invoke-call", 5),
            ("fielded-target", 11),
            ("created-target", 7),
            ("jumped-target", 8),
            ("overwritten-target", 9),
            ("arg-call", 10),
            ("var-call", 12),
            ("end", 0),
        ]),
        "the woven run does not match the family-by-family expectations:\n{}",
        report.render()
    );

    for family in Family::ALL.into_iter().filter(|family| family.supported()) {
        let callback = report
            .woven_marks
            .iter()
            .filter(|(marker, _)| marker == family.callback_marker())
            .count();
        assert_eq!(
            callback,
            1,
            "{} callback must run exactly once:\n{}",
            family.name(),
            report.render()
        );
        let target = report
            .woven_marks
            .iter()
            .filter(|(marker, _)| marker == family.target_marker())
            .count();
        let expected_target = usize::from(family != Family::Cancellable);
        assert_eq!(
            target,
            expected_target,
            "{} target body count:\n{}",
            family.name(),
            report.render()
        );
        let value = report
            .woven_marks
            .iter()
            .find(|(marker, _)| marker == family.callback_marker())
            .map(|(_, value)| *value);
        assert_eq!(
            value,
            Some(family.argument()),
            "{} callback must see its own argument:\n{}",
            family.name(),
            report.render()
        );
    }
}

/// Every family the load-time weave cannot apply is reported with its exact
/// row, its callback never runs, and its target keeps its unmodified marker.
///
/// These are expected-failure assertions: a family that starts working turns
/// the missing skip row into a failure, so the matrix is updated deliberately
/// instead of silently dropping a gap.
#[test]
fn unsupported_families_are_skipped_with_their_exact_reason() {
    let report = report();

    for family in Family::ALL.into_iter().filter(|family| !family.supported()) {
        let rows = report.family_rows(family);
        assert_eq!(
            rows.len(),
            1,
            "{} must have exactly one skip row, got {:#?}\n{}",
            family.name(),
            rows,
            report.render()
        );
        let row = rows[0];
        assert_eq!(
            row.callback,
            family.callback(),
            "{} row names the wrong callback:\n{}",
            family.name(),
            report.render()
        );
        assert_eq!(
            row.at,
            family.skip_at(),
            "{} row names the wrong anchor:\n{}",
            family.name(),
            report.render()
        );
        assert_eq!(
            row.reason,
            family.skip_reason(),
            "{} row has the wrong reason:\n{}",
            family.name(),
            report.render()
        );
        assert!(
            !report.family_is_woven(family),
            "{} must not be woven:\n{}",
            family.name(),
            report.render()
        );
        assert!(
            !report
                .woven_marks
                .iter()
                .any(|(marker, _)| marker == family.callback_marker()),
            "{} callback must not run:\n{}",
            family.name(),
            report.render()
        );

        // The target keeps exactly its own unmodified marker with its own
        // argument; an applied field/new/jump/overwrite/modifyarg/modifyvar
        // would change or replace it.
        let target_value = report
            .woven_marks
            .iter()
            .find(|(marker, _)| marker == family.target_marker())
            .map(|(_, value)| *value);
        let expected = match family {
            // `fielded` writes `field + argument` through its read, and
            // `var` writes `value + 1` before its call.
            Family::Field => family.argument() + 5,
            Family::ModifyVariable => family.argument() + 1,
            _ => family.argument(),
        };
        assert_eq!(
            target_value,
            Some(expected),
            "{} target did not run unmodified:\n{}",
            family.name(),
            report.render()
        );
    }
}

/// The matrix is complete: every family is either woven or skipped, and no
/// unexpected row sneaks into the report.
#[test]
fn the_matrix_covers_every_family_exactly_once() {
    let report = report();
    let table = report.render();
    println!("{table}");

    for family in Family::ALL {
        let woven = report.family_is_woven(family);
        let rows = report.family_rows(family);
        assert!(
            woven ^ (rows.len() == 1),
            "{} must be woven xor skipped exactly once (woven {}, rows {})\n{}",
            family.name(),
            woven,
            rows.len(),
            table
        );
    }

    let known = Family::ALL
        .iter()
        .map(|family| family.mixin_class())
        .collect::<BTreeSet<_>>();
    let unexpected = report
        .rows
        .iter()
        .filter(|row| !known.contains(row.owner.as_str()))
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "unexpected skip rows: {unexpected:#?}\n{table}"
    );
    assert_eq!(
        report.rows.len() + report.woven_plan_count,
        Family::ALL.len(),
        "each family contributes one plan:\n{table}"
    );
}

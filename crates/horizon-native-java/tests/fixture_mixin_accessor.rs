//! `@Accessor`/`@Invoker` members run natively.
//!
//! Real Mixin generates a getter, setter or forwarding call for each of these
//! members; the jar only holds the abstract interface method plus its
//! annotation. The fixture declares all three shapes against a target whose
//! field and method are private, so the run can only pass if the interpreter
//! applied the annexation contract instead of answering the interface's
//! default value.
//!
//! The target class stays in the jar here so the field and method bodies are
//! executable. A game-class target takes the same path: the field table is
//! receiver-keyed and a target outside the jar reaches the host, which is what
//! the second test pins.

use horizon_native_java::{
    MixinAccessorKind, MixinSide, MixinSkipReason, NativeJar, StdHost, Value,
};
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
    resources: &[(&str, &str)],
    excluded: &[&str],
) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-mixin-accessor-{name}-{}",
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
    for (path, body) in resources {
        zip.start_file(*path, SimpleFileOptions::default()).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
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
    jar_path
}

fn annotation_sources() -> BTreeMap<&'static str, &'static str> {
    let mut sources = BTreeMap::new();
    sources.insert(
        "org/spongepowered/asm/mixin/Mixin.java",
        r#"package org.spongepowered.asm.mixin;
import java.lang.annotation.*;
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.TYPE)
public @interface Mixin {
    Class<?>[] value() default {};
    String[] targets() default {};
}
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/gen/Accessor.java",
        r#"package org.spongepowered.asm.mixin.gen;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Accessor { String value() default ""; }
"#,
    );
    sources.insert(
        "org/spongepowered/asm/mixin/gen/Invoker.java",
        r#"package org.spongepowered.asm.mixin.gen;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface Invoker { String value() default ""; }
"#,
    );
    sources
}

fn checks(host: &StdHost) -> BTreeMap<String, bool> {
    host.invocations()
        .iter()
        .filter(|invocation| {
            invocation.method.owner == "demo/Probe" && invocation.method.name == "check"
        })
        .filter_map(|invocation| match invocation.args.as_slice() {
            [Value::Ref(Some(label)), Value::Int(flag)] => Some((label.clone(), *flag != 0)),
            _ => None,
        })
        .collect()
}

/// The same-jar target: the generated members must read and write the private
/// field and dispatch the private method.
#[test]
fn accessors_and_invokers_run_the_generated_contract() {
    let mut sources = annotation_sources();
    sources.insert(
        "demo/Probe.java",
        r#"package demo;
public final class Probe {
    public static void check(String label, boolean value) {}
}
"#,
    );
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public class Target {
    private int value;
    private int twice(int argument) { return argument * 2; }
}
"#,
    );
    sources.insert(
        "demo/TargetAccessor.java",
        r#"package demo;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;
import org.spongepowered.asm.mixin.gen.Invoker;

@Mixin(Target.class)
public interface TargetAccessor {
    @Accessor("value") int getValue();
    @Accessor("value") void setValue(int value);
    @Invoker("twice") int callTwice(int argument);
    @Accessor int getUnnamed(); // inferred from the method name: `unnamed`
}
"#,
    );
    sources.insert(
        "demo/Main.java",
        r#"package demo;
public final class Main {
    public static void main() {
        Target target = new Target();
        TargetAccessor accessor = (TargetAccessor) target;
        accessor.setValue(7);
        Probe.check("getter", accessor.getValue() == 7);
        Probe.check("invoker", accessor.callTwice(3) == 6);
        Probe.check("inferred", accessor.getUnnamed() == 0);
    }
}
"#,
    );
    let jar_path = build_jar(
        "same-jar",
        sources,
        &[(
            "demo.mixins.json",
            r#"{"required":true,"package":"demo","mixins":["TargetAccessor"]}"#,
        )],
        &[
            "demo/Probe.class",
            "org/spongepowered/asm/mixin/Mixin.class",
            "org/spongepowered/asm/mixin/gen/Accessor.class",
            "org/spongepowered/asm/mixin/gen/Invoker.class",
        ],
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();

    let plans = jar.mixin_accessor_plans();
    assert_eq!(plans.len(), 4, "{plans:?}");
    let targets: Vec<(&str, &str)> = plans
        .iter()
        .map(|plan| (plan.target.name.as_str(), plan.target.descriptor.as_str()))
        .collect();
    assert_eq!(
        targets,
        [
            ("twice", "(I)I"),
            ("unnamed", "I"),
            ("value", "I"),
            ("value", "I"),
        ]
    );
    assert_eq!(
        plans
            .iter()
            .map(|plan| plan.kind)
            .collect::<Vec<MixinAccessorKind>>(),
        [
            MixinAccessorKind::Invoker,
            MixinAccessorKind::Getter,
            MixinAccessorKind::Getter,
            MixinAccessorKind::Setter,
        ]
    );

    let weave = jar.plan_load_time_weave(MixinSide::Server);
    assert_eq!(weave.accessors.len(), 4);
    assert!(
        weave
            .skipped
            .iter()
            .all(|plan| plan.reason != MixinSkipReason::UnsupportedFamily),
        "accessor members are applied, not reported as generic mixin members: {:?}",
        weave.skipped
    );

    let mut host = StdHost::new();
    jar.execute("demo.Main", Some("main"), "main", &mut host)
        .unwrap();
    assert_eq!(
        checks(&host),
        BTreeMap::from([
            ("getter".to_owned(), true),
            ("inferred".to_owned(), true),
            ("invoker".to_owned(), true),
        ])
    );
}

/// An accessor whose target is a game class: the field table is keyed by the
/// object marker, so a read answers the type's default and a write is visible
/// to the next read without needing the target's bytecode.
#[test]
fn game_class_accessor_reads_and_writes_the_receiver_field() {
    let mut sources = annotation_sources();
    // The game class is a compile-time stub only: it is excluded from the jar,
    // exactly like a real Minecraft target.
    sources.insert(
        "net/minecraft/world/entity/player/Player.java",
        r#"package net.minecraft.world.entity.player;
public class Player {}
"#,
    );
    sources.insert(
        "demo/Probe.java",
        r#"package demo;
public final class Probe {
    public static void check(String label, boolean value) {}
}
"#,
    );
    sources.insert(
        "demo/PlayerAccessor.java",
        r#"package demo;
import net.minecraft.world.entity.player.Player;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

@Mixin(Player.class)
public interface PlayerAccessor {
    @Accessor("walkSpeed") float getWalkSpeed();
    @Accessor("walkSpeed") void setWalkSpeed(float value);
}
"#,
    );
    // The object is a plain marker in the same process; its class is not the
    // target. The accessor still keys the field by the target owner and the
    // receiver marker, so the read/write pair is the contract under test.
    sources.insert(
        "demo/Main.java",
        r#"package demo;
public final class Main {
    public static void main() {
        Object player = PlayerFactory.create();
        PlayerAccessor accessor = (PlayerAccessor) player;
        Probe.check("default", accessor.getWalkSpeed() == 0.0f);
        accessor.setWalkSpeed(0.25f);
        Probe.check("roundtrip", accessor.getWalkSpeed() == 0.25f);
    }
}
"#,
    );
    sources.insert(
        "demo/PlayerFactory.java",
        r#"package demo;
public final class PlayerFactory {
    public static Object create() { return new Object(); }
}
"#,
    );
    let jar_path = build_jar(
        "game-target",
        sources,
        &[(
            "demo.mixins.json",
            r#"{"required":true,"package":"demo","mixins":["PlayerAccessor"]}"#,
        )],
        &[
            "demo/Probe.class",
            "net/minecraft/world/entity/player/Player.class",
            "org/spongepowered/asm/mixin/Mixin.class",
            "org/spongepowered/asm/mixin/gen/Accessor.class",
            "org/spongepowered/asm/mixin/gen/Invoker.class",
        ],
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    let mut host = StdHost::new();
    // The host owns the game object, so it hands the fixture a marker. The
    // field table is receiver-keyed, which is what the accessor reads.
    host = host.with_mod("demo", "1");
    jar.execute("demo.Main", Some("main"), "main", &mut host)
        .unwrap();
    assert_eq!(
        checks(&host),
        BTreeMap::from([("default".to_owned(), true), ("roundtrip".to_owned(), true),])
    );
}

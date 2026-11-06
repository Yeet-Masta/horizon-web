//! Runtime-built loader fixtures.  These deliberately compile tiny Java mods
//! during the test so the native probe is exercised against real class files,
//! annotations, and loader metadata rather than hand-written bytecode.

use horizon_native_java::{NativeJar, StdHost};
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
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-real-{name}-{}",
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
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(&fs::read(class).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    (root, jar_path)
}

#[test]
fn runtime_built_fabric_mod_executes_initializer() {
    let mut sources = BTreeMap::new();
    sources.insert(
        "net/fabricmc/api/ModInitializer.java",
        "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
    );
    sources.insert("net/fabricmc/loader/api/FabricLoader.java", "package net.fabricmc.loader.api; public final class FabricLoader { public static FabricLoader getInstance() { return new FabricLoader(); } public boolean isModLoaded(String id) { return true; } }");
    sources.insert("demo/FabricRealMod.java", "package demo; import net.fabricmc.api.ModInitializer; import net.fabricmc.loader.api.FabricLoader; public final class FabricRealMod implements ModInitializer { public void onInitialize() { FabricLoader.getInstance().isModLoaded(\"minecraft\"); } }");
    let (root, jar_path) = build_jar(
        "fabric",
        sources,
        "fabric.mod.json",
        br#"{"id":"real-fabric","version":"1.0","entrypoints":{"main":"demo.FabricRealMod"}}"#,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.fabric_id.as_deref(), Some("real-fabric"));
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_built_neoforge_mod_discovers_annotated_entrypoint() {
    let mut sources = BTreeMap::new();
    sources.insert("net/neoforged/fml/common/Mod.java", "package net.neoforged.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }");
    sources.insert("demo/NeoRealMod.java", "package demo; import net.neoforged.fml.common.Mod; @Mod(\"real-neo\") public final class NeoRealMod { public NeoRealMod() {} }");
    let (root, jar_path) = build_jar(
        "neoforge",
        sources,
        "META-INF/neoforge.mods.toml",
        br#"[[mods]]
modId = "real-neo"
version = "1.0"
"#,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("real-neo"));
    assert_eq!(
        jar.discover_entrypoint("neoforge").unwrap().class,
        "demo.NeoRealMod"
    );
    let mut host = jar.seed_host(StdHost::new());
    jar.execute("demo.NeoRealMod", None, "neoforge", &mut host)
        .unwrap();
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_built_legacy_forge_mod_discovers_annotated_entrypoint() {
    let mut sources = BTreeMap::new();
    sources.insert("net/minecraftforge/fml/common/Mod.java", "package net.minecraftforge.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }");
    sources.insert("demo/ForgeRealMod.java", "package demo; import net.minecraftforge.fml.common.Mod; @Mod(\"real-forge\") public final class ForgeRealMod { public ForgeRealMod() {} }");
    let (root, jar_path) = build_jar(
        "forge",
        sources,
        "META-INF/mods.toml",
        br#"modLoader = "javafml"
[[mods]]
modId = "real-forge"
version = "1.0"
"#,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("real-forge"));
    assert_eq!(
        jar.discover_entrypoint("neoforge").unwrap().class,
        "demo.ForgeRealMod"
    );
    let mut host = jar.seed_host(StdHost::new());
    jar.execute("demo.ForgeRealMod", None, "neoforge", &mut host)
        .unwrap();
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_built_mixin_fixture_dispatches_ordered_callback_chain() {
    use horizon_native_java::{MethodRef, MixinInjectionPlan, Value};

    let mut sources = BTreeMap::new();
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public final class Target {
    public static void run(int value) {
        System.getenv("target");
    }
}
"#,
    );
    sources.insert(
        "demo/Callbacks.java",
        r#"package demo;
public final class Callbacks {
    public static void first(int value) {
        System.getenv("first");
    }
    public static void second(int value) {
        System.getenv("second");
    }
}
"#,
    );
    let (root, jar_path) = build_jar(
        "mixin-chain",
        sources,
        "fabric.mod.json",
        br#"{"id":"mixin-chain-fixture","version":"1.0"}"#,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    let target = MethodRef {
        owner: "demo/Target".into(),
        name: "run".into(),
        descriptor: "(I)V".into(),
    };
    let first = MixinInjectionPlan::inject_at_head(
        target.clone(),
        MethodRef {
            owner: "demo/Callbacks".into(),
            name: "first".into(),
            descriptor: "(I)V".into(),
        },
    );
    let second = MixinInjectionPlan::inject_at_head(
        target.clone(),
        MethodRef {
            owner: "demo/Callbacks".into(),
            name: "second".into(),
            descriptor: "(I)V".into(),
        },
    );

    let mut host = StdHost::new();
    let report = jar
        .execute_mixin_chain(&[first, second], None, &[Value::Int(7)], &mut host)
        .unwrap();
    assert_eq!(report.callback_result, Value::Void);
    assert_eq!(report.target_result, Value::Void);

    let getenv = MethodRef {
        owner: "java/lang/System".into(),
        name: "getenv".into(),
        descriptor: "(Ljava/lang/String;)Ljava/lang/String;".into(),
    };
    let markers: Vec<_> = host
        .invocations()
        .iter()
        .filter(|invocation| invocation.method == getenv)
        .filter_map(|invocation| invocation.args.first().cloned())
        .collect();
    assert_eq!(markers.len(), 3);
    let first_marker = Value::Ref(Some("first".into()));
    let target_marker = Value::Ref(Some("target".into()));
    let second_marker = Value::Ref(Some("second".into()));
    assert_eq!(
        markers
            .iter()
            .filter(|marker| **marker == first_marker)
            .count(),
        1
    );
    assert_eq!(
        markers
            .iter()
            .filter(|marker| **marker == target_marker)
            .count(),
        1
    );
    assert_eq!(
        markers
            .iter()
            .filter(|marker| **marker == second_marker)
            .count(),
        1
    );
    let first_index = markers
        .iter()
        .position(|marker| *marker == first_marker)
        .unwrap();
    let second_index = markers
        .iter()
        .position(|marker| *marker == second_marker)
        .unwrap();
    let target_index = markers
        .iter()
        .position(|marker| *marker == target_marker)
        .unwrap();
    assert!(
        first_index < second_index && second_index < target_index,
        "HEAD callbacks must run in declaration order before the target"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_built_mixin_fixture_executes_bounded_injectors() {
    use horizon_native_java::{MethodRef, MixinInjectionPlan, Value};

    let mut sources = BTreeMap::new();
    sources.insert(
        "demo/Target.java",
        r#"package demo;
public final class Target {
    public static void run(int value) {
        int marker = value + 1;
    }
    public static int value(int value) {
        int marker = value + 1;
        return marker;
    }
}
"#,
    );
    sources.insert(
        "demo/Callbacks.java",
        r#"package demo;
public final class Callbacks {
    public static void before(int value) {
        int marker = value + 10;
    }
    public static void after(int value) {
        int marker = value + 100;
    }
    public static void second(int value) {
        int marker = value + 1000;
    }
}
"#,
    );
    let (root, jar_path) = build_jar(
        "mixin",
        sources,
        "fabric.mod.json",
        br#"{"id":"mixin-fixture","version":"1.0"}"#,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    let target = MethodRef {
        owner: "demo/Target".into(),
        name: "run".into(),
        descriptor: "(I)V".into(),
    };
    let callback = MethodRef {
        owner: "demo/Callbacks".into(),
        name: "before".into(),
        descriptor: "(I)V".into(),
    };

    let mut head_host = StdHost::new();
    let head_report = jar
        .execute_mixin_at_head(
            &MixinInjectionPlan::inject_at_head(target.clone(), callback.clone()),
            None,
            &[Value::Int(7)],
            &mut head_host,
        )
        .unwrap();
    assert_eq!(head_report.callback_result, Value::Void);
    assert_eq!(head_report.target_result, Value::Void);
    assert!(head_host.calls().is_empty());

    // A real mod can contribute several callbacks at one hook. The native
    // chain must invoke every callback while running the target exactly once.
    let chain = vec![
        MixinInjectionPlan::inject_at_head(target.clone(), callback.clone()),
        MixinInjectionPlan {
            callback: MethodRef {
                name: "second".into(),
                ..callback.clone()
            },
            ..MixinInjectionPlan::inject_at_head(target.clone(), callback.clone())
        },
    ];
    let chain_report = jar
        .execute_mixin_chain(&chain, None, &[Value::Int(7)], &mut StdHost::new())
        .unwrap();
    assert_eq!(chain_report.callback_result, Value::Void);
    assert_eq!(chain_report.target_result, Value::Void);

    let mut return_host = StdHost::new();
    let return_plan = MixinInjectionPlan {
        target: target.clone(),
        callback: MethodRef {
            name: "after".into(),
            ..callback.clone()
        },
        at: "RETURN".into(),
        cancellable: false,
        invoke_target: None,
        ordinal: None,
        slice: None,
        shift: None,
    };
    let return_report = jar
        .execute_mixin_at_return(&return_plan, None, &[Value::Int(7)], &mut return_host)
        .unwrap();
    assert_eq!(return_report.target_result, Value::Void);
    assert_eq!(return_report.callback_result, Value::Void);
    assert!(return_host.calls().is_empty());

    let tail_plan = MixinInjectionPlan {
        at: "TAIL".into(),
        ..return_plan.clone()
    };
    jar.execute_mixin_at_return(&tail_plan, None, &[Value::Int(7)], &mut StdHost::new())
        .unwrap();

    let non_void_plan = MixinInjectionPlan {
        target: MethodRef {
            name: "value".into(),
            descriptor: "(I)I".into(),
            ..target
        },
        ..return_plan
    };
    let error = jar
        .execute_mixin_at_return(&non_void_plan, None, &[Value::Int(7)], &mut StdHost::new())
        .unwrap_err();
    assert!(error.to_string().contains("return-value callbacks"));
    let _ = fs::remove_dir_all(root);
}

//! A source-built NeoForge jar exercises the forked annotation and metadata
//! path without committing a loader jar or class files to the repository.

use horizon_native_java::{Host, MethodRef, NativeJar, StdHost, Value};
use std::{fs, io::Write, process::Command};
use zip::write::SimpleFileOptions;

#[test]
fn source_built_neoforge_fixture_seeds_loader_and_registration_aliases() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-neoforge-fixture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(src.join("net/neoforged/fml/common")).unwrap();
    fs::write(
        src.join("net/neoforged/fml/common/Mod.java"),
        r#"
package net.neoforged.fml.common;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
@Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }
"#,
    )
    .unwrap();
    fs::write(
        src.join("demo/NeoMod.java"),
        r#"
package demo;
import net.neoforged.fml.common.Mod;
@Mod("neo_fixture") public final class NeoMod { public NeoMod() {} }
"#,
    )
    .unwrap();

    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    fs::create_dir_all(&classes).unwrap();
    let sources = [
        src.join("net/neoforged/fml/common/Mod.java"),
        src.join("demo/NeoMod.java"),
    ];
    let status = Command::new(javac)
        .arg("-source")
        .arg("8")
        .arg("-target")
        .arg("8")
        .arg("-d")
        .arg(&classes)
        .args(sources)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("neoforge.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("META-INF/neoforge.mods.toml", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"[[mods]]
modId = "neo_fixture"
version = "2.0"
[[dependencies.neo_fixture]]
modId = "minecraft"
mandatory = true
[[dependencies.neo_fixture]]
modId = "curios"
mandatory = true
"#,
    )
    .unwrap();
    for entry in files_under(&classes) {
        let name = entry
            .strip_prefix(&classes)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(&fs::read(entry).unwrap()).unwrap();
    }
    zip.finish().unwrap();

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("neo_fixture"));
    assert_eq!(jar.neoforge_version.as_deref(), Some("2.0"));
    assert_eq!(jar.required_mods, ["curios"]);
    assert_eq!(
        jar.discover_entrypoint("neoforge").unwrap().class,
        "demo.NeoMod"
    );

    let mut host = jar.seed_host(StdHost::new());
    jar.execute("demo.NeoMod", None, "neoforge", &mut host)
        .unwrap();
    let mod_list = Value::Ref(Some("neoforge-mod-list".into()));
    let loaded = MethodRef {
        owner: "net/neoforged/fml/ModList".into(),
        name: "isLoaded".into(),
        descriptor: "(Ljava/lang/String;)Z".into(),
    };
    assert_eq!(
        host.invoke_instance(&loaded, &mod_list, &[Value::Ref(Some("curios".into()))])
            .unwrap(),
        Value::Int(1)
    );

    let register = MethodRef {
        owner: "net/neoforged/neoforge/registries/DeferredRegister".into(),
        name: "create".into(),
        descriptor: "(Ljava/lang/Object;Ljava/lang/String;)Lnet/neoforged/neoforge/registries/DeferredRegister;".into(),
    };
    let deferred = host
        .invoke(
            &register,
            &[
                Value::Ref(Some("item".into())),
                Value::Ref(Some("neo_fixture".into())),
            ],
        )
        .unwrap();
    let register_item = MethodRef {
        owner: "net/neoforged/neoforge/registries/DeferredRegister".into(),
        name: "register".into(),
        descriptor: "(Ljava/lang/String;Ljava/util/function/Supplier;)Lnet/neoforged/neoforge/registries/DeferredHolder;".into(),
    };
    host.invoke_instance(
        &register_item,
        &deferred,
        &[
            Value::Ref(Some("crystal".into())),
            Value::Ref(Some("@indy:supplier".into())),
        ],
    )
    .unwrap();
    assert_eq!(host.registrations().len(), 1);
    assert_eq!(host.registrations()[0].id, "crystal");

    let _ = fs::remove_dir_all(root);
}

fn files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
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

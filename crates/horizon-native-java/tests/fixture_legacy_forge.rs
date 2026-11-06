//! A source-built Forge jar keeps the loader and ABI aliases exercised together.
//! The class files are deliberately created in a temporary directory: this is
//! an integration fixture, not a mod shipped with Horizon.

use horizon_native_java::{Host, MethodRef, NativeJar, StdHost, Value};
use std::{fs, io::Write, process::Command};
use zip::write::SimpleFileOptions;

#[test]
fn source_built_legacy_forge_fixture_discovers_mod_and_records_aliases() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-forge-fixture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/fml/common")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/fml/javafmlmod")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/eventbus/api")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/registries")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/network/simple")).unwrap();
    fs::write(
        src.join("net/minecraftforge/fml/common/Mod.java"),
        r#"
package net.minecraftforge.fml.common;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
@Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext.java"),
        r#"
package net.minecraftforge.fml.javafmlmod;
import net.minecraftforge.eventbus.api.IEventBus;
public final class FMLJavaModLoadingContext {
  public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); }
  public IEventBus getModEventBus() { return null; }
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/minecraftforge/eventbus/api/IEventBus.java"),
        r#"
package net.minecraftforge.eventbus.api;
import java.util.function.Consumer;
public interface IEventBus { default IEventBus addListener(Consumer<?> listener) { return this; } }
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/minecraftforge/registries/RegistryObject.java"),
        r#"
package net.minecraftforge.registries;
public final class RegistryObject<T> { }
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/minecraftforge/registries/DeferredRegister.java"),
        r#"
package net.minecraftforge.registries;
import java.util.function.Supplier;
public final class DeferredRegister<T> {
  public static <T> DeferredRegister<T> create(Object registry, String id) { return null; }
  public <V extends T> RegistryObject<V> register(String id, Supplier<V> value) { return null; }
}
"#,
    )
    .unwrap();
    fs::write(src.join("net/minecraftforge/network/simple/SimpleChannel.java"), r#"
package net.minecraftforge.network.simple;
public final class SimpleChannel {
  public SimpleChannel registerMessage(int id, Class<?> type, Object encoder, Object decoder, Object handler) { return this; }
}
"#).unwrap();
    fs::write(
        src.join("demo/LegacyMod.java"),
        r#"
package demo;
import net.minecraftforge.fml.common.Mod;
import net.minecraftforge.fml.javafmlmod.FMLJavaModLoadingContext;
import net.minecraftforge.registries.DeferredRegister;
import net.minecraftforge.network.simple.SimpleChannel;
@Mod("legacy") public final class LegacyMod {
  public LegacyMod() {
    DeferredRegister.create((Object) "item", "legacy").register("crystal", null);
    new SimpleChannel().registerMessage(0, Object.class, null, null, null);
  }
}
"#,
    )
    .unwrap();

    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    let sources = fs::read_dir(src.join("demo"))
        .unwrap()
        .chain(fs::read_dir(src.join("net/minecraftforge/fml/common")).unwrap())
        .chain(fs::read_dir(src.join("net/minecraftforge/fml/javafmlmod")).unwrap())
        .chain(fs::read_dir(src.join("net/minecraftforge/eventbus/api")).unwrap())
        .chain(fs::read_dir(src.join("net/minecraftforge/registries")).unwrap())
        .chain(fs::read_dir(src.join("net/minecraftforge/network/simple")).unwrap())
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    fs::create_dir_all(&classes).unwrap();
    let status = Command::new(javac)
        .arg("-source")
        .arg("8")
        .arg("-target")
        .arg("8")
        .arg("-d")
        .arg(&classes)
        .args(&sources)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("legacy.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("META-INF/mods.toml", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"modLoader = "javafml"
license = "MIT"
[[mods]]
modId = "legacy"
version = "1.0"
[[dependencies.legacy]]
modId = "minecraft"
mandatory = true
"#,
    )
    .unwrap();
    for entry in walkdir(&classes) {
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
    assert_eq!(jar.neoforge_id.as_deref(), Some("legacy"));
    assert_eq!(
        jar.discover_entrypoint("neoforge").unwrap().class,
        "demo.LegacyMod"
    );
    let mut host = StdHost::new().with_mod("legacy", "1.0");
    jar.execute("demo.LegacyMod", None, "neoforge", &mut host)
        .unwrap();
    let deferred = host.invoke(&MethodRef { owner: "net/minecraftforge/registries/DeferredRegister".into(), name: "create".into(), descriptor: "(Ljava/lang/Object;Ljava/lang/String;)Lnet/minecraftforge/registries/DeferredRegister;".into() }, &[Value::Ref(Some("item".into())), Value::Ref(Some("legacy".into()))]).unwrap();
    host.invoke_instance(&MethodRef { owner: "net/minecraftforge/registries/DeferredRegister".into(), name: "register".into(), descriptor: "(Ljava/lang/String;Ljava/util/function/Supplier;)Lnet/minecraftforge/registries/RegistryObject;".into() }, &deferred, &[Value::Ref(Some("crystal".into())), Value::Ref(Some("@indy:supplier".into()))]).unwrap();
    host.invoke_instance(&MethodRef { owner: "net/minecraftforge/network/simple/SimpleChannel".into(), name: "registerMessage".into(), descriptor: "(ILjava/lang/Class;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Lnet/minecraftforge/network/simple/SimpleChannel;".into() }, &Value::Ref(Some("channel".into())), &[Value::Int(0), Value::Ref(Some("packet".into())), Value::Ref(Some("encoder".into())), Value::Ref(Some("decoder".into())), Value::Ref(Some("handler".into()))]).unwrap();
    assert_eq!(host.registrations().len(), 1);
    assert_eq!(host.payload_registrations().len(), 1);
    assert_eq!(host.registrations()[0].id, "crystal");
    assert_eq!(
        host.payload_registrations()[0].direction,
        "playBidirectional"
    );
    let _ = fs::remove_dir_all(root);
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walkdir(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

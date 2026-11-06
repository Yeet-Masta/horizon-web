//! A source-built legacy Forge fixture that crosses the event-bus boundary
//! with a real invokedynamic listener marker.

use horizon_native_java::{MethodRef, NativeJar, StdHost, Value};
use std::{fs, io::Write, process::Command};
use zip::write::SimpleFileOptions;

#[test]
fn source_built_legacy_forge_fixture_registers_and_dispatches_listener() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-forge-event-fixture-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    fs::create_dir_all(src.join("demo")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/fml/common")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/fml/javafmlmod")).unwrap();
    fs::create_dir_all(src.join("net/minecraftforge/eventbus/api")).unwrap();
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
        src.join("net/minecraftforge/eventbus/api/IEventBus.java"),
        r#"
package net.minecraftforge.eventbus.api;
import java.util.function.Consumer;
public interface IEventBus { default IEventBus addListener(Consumer<?> listener) { return this; } }
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
        src.join("demo/LegacyEventMod.java"),
        r#"
package demo;
import net.minecraftforge.fml.common.Mod;
import net.minecraftforge.fml.javafmlmod.FMLJavaModLoadingContext;
import net.minecraftforge.eventbus.api.IEventBus;
@Mod("legacy_event") public final class LegacyEventMod {
  public LegacyEventMod() {
    IEventBus bus = FMLJavaModLoadingContext.get().getModEventBus();
    bus.addListener(value -> { });
  }
}
"#,
    )
    .unwrap();

    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    let sources = vec![
        src.join("demo/LegacyEventMod.java"),
        src.join("net/minecraftforge/fml/common/Mod.java"),
        src.join("net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext.java"),
        src.join("net/minecraftforge/eventbus/api/IEventBus.java"),
    ];
    fs::create_dir_all(&classes).unwrap();
    let status = Command::new(javac)
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(&sources)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("legacy-event.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("META-INF/mods.toml", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"modLoader = "javafml"
license = "MIT"
[[mods]]
modId = "legacy_event"
version = "1.0"
[[dependencies.legacy_event]]
modId = "minecraft"
mandatory = true
"#,
    )
    .unwrap();
    let class_path = classes.join("demo/LegacyEventMod.class");
    zip.start_file("demo/LegacyEventMod.class", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&fs::read(class_path).unwrap()).unwrap();
    zip.finish().unwrap();

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("legacy_event"));
    assert_eq!(
        jar.discover_entrypoint("neoforge").unwrap().class,
        "demo.LegacyEventMod"
    );
    let mut host = jar.seed_host(StdHost::new());
    jar.execute("demo.LegacyEventMod", None, "neoforge", &mut host)
        .unwrap();
    assert_eq!(host.listener_registrations().count(), 1);

    let dispatches = host.dispatch_event(Value::Ref(Some("forge-event".into())));
    assert_eq!(dispatches.len(), 1);
    assert_eq!(
        dispatches[0].listener.method.owner,
        "net/minecraftforge/eventbus/api/IEventBus"
    );
    assert_eq!(dispatches[0].event, Value::Ref(Some("forge-event".into())));
    let event_bus = MethodRef {
        owner: "net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext".into(),
        name: "getModEventBus".into(),
        descriptor: "()Lnet/minecraftforge/eventbus/api/IEventBus;".into(),
    };
    assert!(host.calls().iter().any(|call| call == &event_bus));
    let _ = fs::remove_dir_all(root);
}

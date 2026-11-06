//! A source-built legacy Forge fixture that crosses the bounded capability and
//! `LazyOptional` boundary with a real constructor and lambda callback.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{fs, io::Write, process::Command};
use zip::write::SimpleFileOptions;

#[test]
fn source_built_legacy_forge_fixture_reads_seeded_capability() {
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-forge-capability-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let classes = root.join("classes");
    for package in [
        "demo",
        "net/minecraftforge/fml/common",
        "net/minecraft/world/level",
        "net/minecraftforge/common/capabilities",
        "net/minecraftforge/common/util",
    ] {
        fs::create_dir_all(src.join(package)).unwrap();
    }
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
        src.join("net/minecraftforge/common/capabilities/Capability.java"),
        "package net.minecraftforge.common.capabilities; public final class Capability<T> {}\n",
    )
    .unwrap();
    fs::write(
        src.join("net/minecraftforge/common/util/LazyOptional.java"),
        r#"
package net.minecraftforge.common.util;
import java.util.function.Consumer;
public final class LazyOptional<T> {
  public boolean isPresent() { return false; }
  public void ifPresent(Consumer<? super T> consumer) {}
  public T orElse(T other) { return other; }
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("net/minecraft/world/level/Level.java"),
        r#"
package net.minecraft.world.level;
import net.minecraftforge.common.capabilities.Capability;
import net.minecraftforge.common.util.LazyOptional;
public class Level {
  public <T> LazyOptional<T> getCapability(Capability<T> capability, Object side) { return new LazyOptional<>(); }
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("demo/CapabilityMod.java"),
        r#"
package demo;
import net.minecraftforge.fml.common.Mod;
import net.minecraft.world.level.Level;
import net.minecraftforge.common.capabilities.Capability;
import net.minecraftforge.common.util.LazyOptional;
@Mod("legacy_capability") public final class CapabilityMod {
  public CapabilityMod() {
    Level level = new Level();
    Capability<Object> capability = new Capability<>();
    LazyOptional<Object> value = level.getCapability(capability, "NORTH");
    if (!value.isPresent()) throw new IllegalStateException("seeded capability missing");
    // The bounded host resolves LazyOptional contents through orElse/get; it
    // cannot run an arbitrary Java Consumer, so the callback form stays on the
    // JVM fallback and the value check uses the supported accessor.
    Object item = value.orElse("missing");
    if (!item.toString().equals("energy")) throw new IllegalStateException("wrong capability value");
  }
}
"#,
    )
    .unwrap();

    let javac = std::env::var_os("JAVA_HOME")
        .map(|home| std::path::PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into());
    fs::create_dir_all(&classes).unwrap();
    let sources = [
        src.join("demo/CapabilityMod.java"),
        src.join("net/minecraftforge/fml/common/Mod.java"),
        src.join("net/minecraftforge/common/capabilities/Capability.java"),
        src.join("net/minecraftforge/common/util/LazyOptional.java"),
        src.join("net/minecraft/world/level/Level.java"),
    ];
    let status = Command::new(javac)
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(&sources)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed");

    let jar_path = root.join("legacy-capability.jar");
    let file = fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("META-INF/mods.toml", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        br#"modLoader = "javafml"
[[mods]]
modId = "legacy_capability"
version = "1.0"
"#,
    )
    .unwrap();
    // Keep the loader and Minecraft API stubs off the jar. The native host must
    // satisfy these calls through its bounded Forge façade.
    zip.start_file("demo/CapabilityMod.class", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&fs::read(classes.join("demo/CapabilityMod.class")).unwrap())
        .unwrap();
    zip.finish().unwrap();

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("legacy_capability"));
    let mut host = jar.seed_host(StdHost::new());
    host.seed_capability(
        "net/minecraft/world/level/Level",
        "net/minecraftforge/common/capabilities/Capability#1",
        None::<String>,
        Some("NORTH"),
        Value::Ref(Some("energy".into())),
    );
    jar.execute("demo.CapabilityMod", None, "neoforge", &mut host)
        .unwrap();
    assert_eq!(host.capability_requests().len(), 1);
    assert_eq!(
        host.capability_requests()[0].owner,
        "net/minecraft/world/level/Level"
    );
    assert_eq!(host.capability_requests()[0].side.as_deref(), Some("NORTH"));

    let _ = fs::remove_dir_all(root);
}

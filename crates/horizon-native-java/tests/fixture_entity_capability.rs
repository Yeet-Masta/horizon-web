//! A source-built Forge/NeoForge fixture that combines the constructor
//! entrypoint, event-bus registration, and an entity capability lookup.
//!
//! Entity capabilities use the same bounded `LazyOptional` contract as level
//! and block-entity capabilities, but the owner is part of the request key.
//! The fixture first discovers the host marker for its opaque capability, then
//! reruns with that marker seeded to a deterministic value.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use zip::write::SimpleFileOptions;

const MOD_CLASS: &str = "demo.EntityCapabilityMod";
const MOD_ID: &str = "entity_capability";
const ENTITY: &str = "net/minecraft/world/entity/Entity";
const BUS: &str = "net/neoforged/bus/api/IEventBus";

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn scratch_root(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-entity-capability-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

fn write_source(root: &Path, relative: &str, body: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn build_jar(name: &str) -> (PathBuf, NativeJar) {
    let root = scratch_root(name);
    let src = root.join("src");
    let classes = root.join("classes");
    let sources = [
        (
            "net/neoforged/fml/common/Mod.java",
            r#"package net.neoforged.fml.common;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
@Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }
"#,
        ),
        (
            "net/neoforged/bus/api/IEventBus.java",
            r#"package net.neoforged.bus.api;
import java.util.function.Consumer;
public interface IEventBus {
  default IEventBus addListener(Consumer<?> listener) { return this; }
}
"#,
        ),
        (
            "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext.java",
            r#"package net.neoforged.fml.javafmlmod;
import net.neoforged.bus.api.IEventBus;
public final class FMLJavaModLoadingContext {
  public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); }
  public IEventBus getModEventBus() { return null; }
}
"#,
        ),
        (
            "net/neoforged/neoforge/capabilities/Capability.java",
            "package net.neoforged.neoforge.capabilities; public final class Capability<T> {}\n",
        ),
        (
            "net/neoforged/neoforge/common/util/LazyOptional.java",
            r#"package net.neoforged.neoforge.common.util;
public final class LazyOptional<T> {
  public boolean isPresent() { return false; }
  public T orElse(T other) { return other; }
}
"#,
        ),
        (
            "net/minecraft/world/entity/Entity.java",
            r#"package net.minecraft.world.entity;
import net.neoforged.neoforge.capabilities.Capability;
import net.neoforged.neoforge.common.util.LazyOptional;
public class Entity {
  public <T> LazyOptional<T> getCapability(Capability<T> capability, String side) {
    return new LazyOptional<>();
  }
}
"#,
        ),
        (
            "demo/EntityCapabilityMod.java",
            r#"package demo;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext;
import net.neoforged.neoforge.capabilities.Capability;
import net.neoforged.neoforge.common.util.LazyOptional;
import net.minecraft.world.entity.Entity;
@Mod("entity_capability") public final class EntityCapabilityMod {
  public EntityCapabilityMod() {
    IEventBus bus = FMLJavaModLoadingContext.get().getModEventBus();
    bus.addListener(value -> { });
    Entity entity = new Entity();
    Capability<Object> capability = new Capability<>();
    LazyOptional<Object> found = entity.getCapability(capability, "NORTH");
    if (!found.isPresent()) throw new IllegalStateException("seeded capability missing");
    Object value = found.orElse("missing");
    if (!value.toString().equals("energy")) throw new IllegalStateException("wrong capability value");
  }
}
"#,
        ),
    ];
    let paths = sources
        .iter()
        .map(|(relative, body)| write_source(&src, relative, body))
        .collect::<Vec<_>>();
    fs::create_dir_all(&classes).unwrap();
    let status = Command::new(javac())
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&classes)
        .args(&paths)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed for {name}");

    let jar_path = root.join(format!("{name}.jar"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    zip.start_file("META-INF/neoforge.mods.toml", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(
        format!("modLoader = \"javafml\"\n[[mods]]\nmodId = \"{MOD_ID}\"\nversion = \"1.0\"\n")
            .as_bytes(),
    )
    .unwrap();
    zip.start_file(
        "demo/EntityCapabilityMod.class",
        SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(&fs::read(classes.join("demo/EntityCapabilityMod.class")).unwrap())
        .unwrap();
    zip.finish().unwrap();

    (root, NativeJar::from_path(&jar_path).unwrap())
}

#[test]
fn entity_capability_entrypoint_registers_and_dispatches_event_listener() {
    let (root, jar) = build_jar("seeded");
    assert_eq!(jar.neoforge_id.as_deref(), Some(MOD_ID));
    assert_eq!(
        jar.discover_entrypoint("neoforge").unwrap().class,
        MOD_CLASS
    );

    // The unseeded run still executes the constructor far enough to record the
    // event listener and reveal the opaque capability marker.
    let mut probe = jar.seed_host(StdHost::new());
    let error = jar
        .execute(MOD_CLASS, None, "neoforge", &mut probe)
        .expect_err("an unseeded entity capability must stay empty");
    assert!(format!("{error:?}").contains("seeded capability missing"));
    assert_eq!(probe.capability_requests().len(), 1);
    assert_eq!(probe.capability_requests()[0].owner, ENTITY);
    assert_eq!(
        probe.capability_requests()[0].side.as_deref(),
        Some("NORTH")
    );
    assert_eq!(probe.listener_registrations().count(), 1);

    let capability = probe.capability_requests()[0].capability.clone();
    let mut host = jar.seed_host(StdHost::new());
    host.seed_capability(
        ENTITY,
        &capability,
        None::<String>,
        Some("NORTH"),
        Value::Ref(Some("energy".into())),
    );
    jar.execute(MOD_CLASS, None, "neoforge", &mut host)
        .expect("the seeded entity capability should resolve");
    assert_eq!(host.capability_requests().len(), 1);
    assert_eq!(host.capability_requests()[0].owner, ENTITY);
    assert_eq!(host.listener_registrations().count(), 1);
    let dispatches = host.dispatch_event(Value::Ref(Some("entity-event".into())));
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0].listener.method.owner, BUS);
    assert_eq!(dispatches[0].event, Value::Ref(Some("entity-event".into())));

    let _ = fs::remove_dir_all(root);
}

//! A source-built NeoForge fixture that crosses the bounded NeoForge capability
//! surface: `Capability`, `LazyOptional`, `IItemHandler` and the
//! `Level`/`BlockEntity#getCapability` adapters.
//!
//! The mod allocates a capability, asks a `Level` for it on one side and a
//! `BlockEntity` on another, reads the resolved `IItemHandler` through
//! `getSlots`/`getStackInSlot`/`extractItem`, and falls back through `orElse`
//! for a side nobody seeded. Only the mod's own class files are packaged; every
//! API stub stays off the jar, so each call has to cross the host façade,
//! exactly like a real install where NeoForge is a host surface.
//!
//! The host names a constructed object `<internal/class/name>#<n>` with `n`
//! allocated per invocation, so the marker a `new Capability<>()` receives is an
//! allocation-order artifact, not a stable name. This fixture therefore runs the
//! mod once against an unseeded host to *discover* the markers the host assigned,
//! then seeds those exact markers and executes again for the real assertions.
//! Hard-coding an ordinal would bind the test to this fixture's allocation order
//! and break the moment it allocates one more object.

use horizon_native_java::{NativeJar, StdHost, Value};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use zip::write::SimpleFileOptions;

const MOD_CLASS: &str = "demo.CapabilityMod";
const MOD_ID: &str = "neoforge_capability";
const LEVEL: &str = "net/minecraft/world/level/Level";
const BLOCK_ENTITY: &str = "net/minecraft/world/level/block/entity/BlockEntity";
const ITEM_HANDLER: &str = "net/neoforged/neoforge/items/IItemHandler";
/// The host-owned item-handler marker the seeded capability resolves to.
const HANDLER: &str = "handler/items";

fn javac() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin/javac"))
        .unwrap_or_else(|| "javac".into())
}

fn scratch_root(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-neoforge-capability-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

/// The host surface the mod compiles against: NeoForge capability classes plus
/// the Minecraft classes their signatures mention.
fn stubs() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "net/neoforged/fml/common/Mod.java",
            "package net.neoforged.fml.common; \
             @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) \
             @java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE) \
             public @interface Mod { String value(); }",
        ),
        (
            "net/neoforged/neoforge/capabilities/Capability.java",
            "package net.neoforged.neoforge.capabilities; public final class Capability<T> {}",
        ),
        (
            "net/neoforged/neoforge/common/util/LazyOptional.java",
            r#"package net.neoforged.neoforge.common.util;
public final class LazyOptional<T> {
  public boolean isPresent() { return false; }
  public boolean isEmpty() { return true; }
  public T orElse(T other) { return other; }
  public T get() { return null; }
}"#,
        ),
        (
            "net/neoforged/neoforge/items/IItemHandler.java",
            r#"package net.neoforged.neoforge.items;
import net.minecraft.world.item.ItemStack;
public interface IItemHandler {
  int getSlots();
  ItemStack getStackInSlot(int slot);
  ItemStack extractItem(int slot, int amount, boolean simulate);
}"#,
        ),
        (
            "net/minecraft/world/item/ItemStack.java",
            "package net.minecraft.world.item; public class ItemStack {}",
        ),
        (
            "net/minecraft/core/BlockPos.java",
            "package net.minecraft.core; public class BlockPos { public BlockPos(int x, int y, int z) {} }",
        ),
        (
            "net/minecraft/world/level/Level.java",
            "package net.minecraft.world.level; public class Level { \
             public <T> net.neoforged.neoforge.common.util.LazyOptional<T> getCapability(\
             net.neoforged.neoforge.capabilities.Capability<T> capability, \
             net.minecraft.core.BlockPos position, String side) { \
             return new net.neoforged.neoforge.common.util.LazyOptional<T>(); } }",
        ),
        (
            "net/minecraft/world/level/block/entity/BlockEntity.java",
            "package net.minecraft.world.level.block.entity; public class BlockEntity { \
             public <T> net.neoforged.neoforge.common.util.LazyOptional<T> getCapability(\
             net.neoforged.neoforge.capabilities.Capability<T> capability, String side) { \
             return new net.neoforged.neoforge.common.util.LazyOptional<T>(); } }",
        ),
    ]
}

/// The mod: capability lookup, `LazyOptional` unwrapping, item-handler reads and
/// an `orElse` fallback for a side nobody seeded.
const CAPABILITY_MOD: &str = r#"
package demo;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.capabilities.Capability;
import net.neoforged.neoforge.common.util.LazyOptional;
import net.neoforged.neoforge.items.IItemHandler;
@Mod("neoforge_capability") public final class CapabilityMod {
  public CapabilityMod() {
    // Allocate every opaque object before the first query: the host's markers
    // are per-invocation, so allocating inside a branch would shift them.
    Level level = new Level();
    BlockEntity entity = new BlockEntity();
    BlockPos position = new BlockPos(1, 2, 3);
    Capability<Object> capability = new Capability<>();

    LazyOptional<Object> found = level.getCapability(capability, position, "NORTH");
    if (found.isPresent()) {
      IItemHandler handler = (IItemHandler) found.orElse(null);
      if (handler == null) throw new IllegalStateException("resolved capability has no value");
      if (handler.getSlots() != 2) throw new IllegalStateException("wrong slot count");
      Object first = handler.getStackInSlot(0);
      if (first == null || !first.toString().equals("stack/iron")) throw new IllegalStateException("wrong slot 0");
      Object second = handler.getStackInSlot(1);
      if (second != null) throw new IllegalStateException("slot 1 should be empty");
      // The extraction proves the bridge reached the handler: it clears the
      // seeded slot, which the Rust side observes afterwards.
      Object extracted = handler.extractItem(0, 1, false);
      if (extracted == null || !extracted.toString().equals("stack/iron")) throw new IllegalStateException("bad extract");
    }

    // Negative case, unconditional: an unseeded side must resolve to an empty
    // LazyOptional and `orElse` must hand back the fallback instead of throwing
    // the "seeded capability missing" error. Reference identity is the check:
    // the only way to obtain `level` back is through the fallback argument.
    LazyOptional<Object> missing = entity.getCapability(capability, "SOUTH");
    if (missing.isPresent() || !missing.isEmpty()) throw new IllegalStateException("unseeded capability resolved");
    Object fallback = missing.orElse(level);
    if (fallback != level) throw new IllegalStateException("orElse ignored the fallback value");
    if (missing.get() != null) throw new IllegalStateException("empty LazyOptional returned a value");
  }
}
"#;

fn build_jar(name: &str) -> (PathBuf, NativeJar) {
    let root = scratch_root(name);
    let (src, classes) = (root.join("src"), root.join("classes"));
    let mut sources = stubs()
        .into_iter()
        .map(|(relative, body)| (relative.to_owned(), body))
        .collect::<Vec<_>>();
    sources.push(("demo/CapabilityMod.java".to_owned(), CAPABILITY_MOD));
    let paths = sources
        .iter()
        .map(|(relative, body)| {
            let path = src.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        })
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
    // Keep the loader and Minecraft API stubs off the jar: the native host must
    // satisfy those calls through its bounded NeoForge façade. The fixture mod
    // has no lambdas, so it compiles to exactly one class file.
    zip.start_file("demo/CapabilityMod.class", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&fs::read(classes.join("demo/CapabilityMod.class")).unwrap())
        .unwrap();
    zip.finish().unwrap();

    (root, NativeJar::from_path(&jar_path).unwrap())
}

/// Run the mod against an unseeded host and report the markers the host handed
/// out for the `Capability` and `BlockPos` the mod constructed.
fn discover_markers(jar: &NativeJar) -> (String, Option<String>) {
    let mut probe = jar.seed_host(StdHost::new());
    jar.execute(MOD_CLASS, None, "neoforge", &mut probe)
        .unwrap();
    let requests = probe.capability_requests();
    assert_eq!(requests.len(), 2);
    // Without a seed the positive branch stays closed, so the handler is never
    // reached and nothing can silently pass by reading an unseeded inventory.
    assert!(
        !probe
            .invocations()
            .iter()
            .any(|invocation| invocation.method.owner == ITEM_HANDLER),
        "an unseeded capability must not resolve an item handler"
    );
    let north = &requests[0];
    assert_eq!(north.owner, LEVEL);
    assert_eq!(north.side.as_deref(), Some("NORTH"));
    assert!(
        north.position.is_some(),
        "the three-argument Level call carries a position marker"
    );
    assert!(
        north
            .capability
            .starts_with("net/neoforged/neoforge/capabilities/Capability#"),
        "unexpected capability marker {}",
        north.capability
    );
    (north.capability.clone(), north.position.clone())
}

/// A host seeded for one `side` only: the resolved capability plus its slots.
fn seeded_host(jar: &NativeJar, capability: &str, position: Option<String>, side: &str) -> StdHost {
    let mut host = jar.seed_host(StdHost::new());
    host.seed_capability(
        LEVEL,
        capability,
        position,
        Some(side),
        Value::Ref(Some(HANDLER.into())),
    );
    host.seed_item_handler(
        HANDLER,
        [Value::Ref(Some("stack/iron".into())), Value::Ref(None)],
    );
    host
}

#[test]
fn source_built_neoforge_fixture_reads_seeded_capability_and_item_handler() {
    let (root, jar) = build_jar("seeded");
    assert_eq!(jar.neoforge_id.as_deref(), Some(MOD_ID));

    let (capability, position) = discover_markers(&jar);
    let mut host = seeded_host(&jar, &capability, position.clone(), "NORTH");
    jar.execute(MOD_CLASS, None, "neoforge", &mut host).unwrap();

    let requests = host.capability_requests();
    assert_eq!(requests.len(), 2);
    let north = &requests[0];
    assert_eq!(north.owner, LEVEL);
    assert_eq!(north.capability, capability);
    assert_eq!(north.position.as_deref(), position.as_deref());
    assert_eq!(north.side.as_deref(), Some("NORTH"));

    // The negative case in the same run: same `Capability` instance, a distinct
    // owner and side, and no position because `BlockEntity` takes two arguments.
    let south = &requests[1];
    assert_eq!(south.owner, BLOCK_ENTITY);
    assert_eq!(south.capability, capability);
    assert_eq!(south.position, None);
    assert_eq!(south.side.as_deref(), Some("SOUTH"));

    // The seeded value crossed back as the host-owned handler marker, then the
    // mod read both slots and extracted slot 0.
    let calls = host
        .invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == ITEM_HANDLER)
        .collect::<Vec<_>>();
    let names = calls
        .iter()
        .map(|invocation| invocation.method.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        names.join(","),
        "getSlots,getStackInSlot,getStackInSlot,extractItem"
    );
    for invocation in &calls {
        assert_eq!(
            invocation.receiver,
            Some(Value::Ref(Some(HANDLER.into()))),
            "every handler call goes to the seeded handler marker"
        );
    }
    assert_eq!(
        calls[1].args,
        [Value::Int(0)],
        "slot 0 is the slot the fixture reads first"
    );
    assert_eq!(calls[2].args, [Value::Int(1)]);
    assert_eq!(
        calls[3].args,
        [Value::Int(0), Value::Int(1), Value::Int(0)],
        "extractItem(slot 0, amount 1, simulate false)"
    );
    // The extraction is visible in the host's slot table: slot 0 held the iron
    // stack the mod validated and is now empty.
    assert_eq!(
        host.item_handler_slots(HANDLER).unwrap(),
        &[Value::Ref(None), Value::Ref(None)]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn unseeded_side_stays_empty_and_or_else_returns_the_fallback() {
    let (root, jar) = build_jar("unseeded-side");
    let (capability, position) = discover_markers(&jar);

    // Seed a side the mod never asks for. Both real requests (NORTH on the level,
    // SOUTH on the block entity) must then resolve to empty optionals: the seed
    // is keyed by owner, capability, position and side, not by capability alone.
    let mut host = seeded_host(&jar, &capability, position, "EAST");
    // The mod's `orElse(level)` identity check runs unconditionally and is the
    // reason this must succeed: it would have thrown "seeded capability missing"
    // had the empty optional been reported as present, and "orElse ignored the
    // fallback value" had the fallback not been handed back.
    jar.execute(MOD_CLASS, None, "neoforge", &mut host).unwrap();

    let requests = host.capability_requests();
    let sides = requests
        .iter()
        .map(|request| request.side.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(sides, [Some("NORTH"), Some("SOUTH")]);
    assert!(
        !host
            .invocations()
            .iter()
            .any(|invocation| invocation.method.owner == ITEM_HANDLER),
        "an empty LazyOptional must not resolve an item handler"
    );
    // The seed itself is untouched: nothing consumed it.
    assert_eq!(
        host.item_handler_slots(HANDLER).unwrap(),
        &[Value::Ref(Some("stack/iron".into())), Value::Ref(None)]
    );

    let _ = fs::remove_dir_all(root);
}

//! A broader "mod catalog" fixture.  Each test compiles a small but realistic
//! Fabric, NeoForge, or legacy Forge mod from Java source at test time and runs
//! its entrypoint against the existing bounded native host.
//!
//! Loader APIs (`FabricLoader`, `DeferredRegister`, the event bus, ...) are
//! compiled to a separate classpath directory and are deliberately *not*
//! packaged in the jar: that mirrors a real install, where the loader is a host
//! surface and the jar only carries the mod's own classes.  Every call into a
//! loader API therefore crosses `Host` and is observable on `StdHost`.
//!
//! The helpers are a self-contained copy of `real_mod_runtime.rs` with one
//! addition: `build_catalog_jar` compiles stubs separately from the mod.

use horizon_native_java::{HostInvocation, NativeError, NativeJar, StdHost, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use zip::write::SimpleFileOptions;

const IDENTIFIER: &str = "net/minecraft/resources/Identifier";
const FABRIC_LOADER: &str = "net/fabricmc/loader/api/FabricLoader";
const MC_REGISTRY: &str = "net/minecraft/core/Registry";
const NEO_DEFERRED: &str = "net/neoforged/neoforge/registries/DeferredRegister";
const NEO_PAYLOAD_EVENT: &str = "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent";
const FORGE_DEFERRED: &str = "net/minecraftforge/registries/DeferredRegister";
const FORGE_CHANNEL: &str = "net/minecraftforge/network/simple/SimpleChannel";

/// A Java source tree for `build_catalog_jar`: relative path => source text.
macro_rules! sources {
    ($($path:literal => $body:expr),* $(,)?) => {{
        let mut map: BTreeMap<&str, &str> = BTreeMap::new();
        $(map.insert($path, $body);)*
        map
    }};
}

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

fn scratch_root(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "horizon-native-java-catalog-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

fn write_sources(root: &Path, tree: &BTreeMap<&str, &str>) -> Vec<PathBuf> {
    tree.iter()
        .map(|(relative, body)| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            path
        })
        .collect()
}

fn compile(out: &Path, classpath: Option<&Path>, sources: &[PathBuf], label: &str) {
    fs::create_dir_all(out).unwrap();
    let mut command = Command::new(javac());
    command.arg("--release").arg("17").arg("-d").arg(out);
    if let Some(classpath) = classpath {
        command.arg("-cp").arg(classpath);
    }
    let status = command.args(sources).status().unwrap();
    assert!(status.success(), "javac failed for {label}");
}

/// Compile loader stubs onto a classpath, compile the mod against them, and
/// package only the mod's own class files (plus its loader metadata).
fn build_catalog_jar(
    name: &str,
    stubs: &BTreeMap<&str, &str>,
    sources: &BTreeMap<&str, &str>,
    metadata_path: &str,
    metadata: &[u8],
) -> (PathBuf, PathBuf) {
    let root = scratch_root(name);
    let stub_classes = root.join("stub-classes");
    let classes = root.join("classes");
    if !stubs.is_empty() {
        let paths = write_sources(&root.join("stub-src"), stubs);
        compile(&stub_classes, None, &paths, name);
    }
    let paths = write_sources(&root.join("src"), sources);
    compile(&classes, Some(&stub_classes), &paths, name);

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

fn calls_to<'a>(host: &'a StdHost, owner: &str, name: &str) -> Vec<&'a HostInvocation> {
    host.invocations()
        .iter()
        .filter(|invocation| invocation.method.owner == owner && invocation.method.name == name)
        .collect()
}

macro_rules! assert_called {
    ($host:expr, $owner:expr, $name:expr, $times:expr) => {{
        let found = calls_to($host, $owner, $name);
        assert_eq!(
            found.len(),
            $times,
            "expected {} x {}#{}",
            $times,
            $owner,
            $name
        );
    }};
}

#[test]
fn fabric_catalog_mod_initializes_items_blocks_and_loader_queries() {
    let stubs = sources! {
        "net/fabricmc/api/ModInitializer.java" => "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
        "net/fabricmc/loader/api/ModContainer.java" => "package net.fabricmc.loader.api; public interface ModContainer { String getModId(); String getVersion(); }",
        "net/fabricmc/loader/api/FabricLoader.java" => r#"package net.fabricmc.loader.api; import java.util.List; import java.util.Optional; public final class FabricLoader { public static FabricLoader getInstance() { return new FabricLoader(); } public Optional<ModContainer> getModContainer(String id) { return Optional.empty(); } public List<ModContainer> getAllMods() { return List.of(); } public boolean isModLoaded(String id) { return false; } }"#,
        "net/minecraft/resources/Identifier.java" => r#"package net.minecraft.resources; public final class Identifier { private final String namespace; private final String path; private Identifier(String n, String p) { namespace = n; path = p; } public static Identifier fromNamespaceAndPath(String n, String p) { return new Identifier(n, p); } public static Identifier parse(String v) { return new Identifier("minecraft", v); } public static Identifier tryParse(String v) { return parse(v); } public String getNamespace() { return namespace; } public String getPath() { return path; } public String toString() { return namespace + ":" + path; } }"#,
        "net/minecraft/core/Registry.java" => "package net.minecraft.core; import net.minecraft.resources.Identifier; public interface Registry<T> { static <T> T register(Registry<T> registry, Identifier id, T value) { return value; } }",
        "net/minecraft/core/registries/Registries.java" => "package net.minecraft.core.registries; import net.minecraft.core.Registry; public final class Registries { public static final Registry<Object> ITEM = null; public static final Registry<Object> BLOCK = null; }",
    };
    // Content classes ship with the jar so `new Item(...)` keeps a real object
    // identity; only the loader/registry surfaces stay host-side.
    let sources = sources! {
        "net/minecraft/world/item/Item.java" => r#"package net.minecraft.world.item; public class Item { public Item(Properties p) { } public static class Properties { public Properties() { } public Properties stacksTo(int n) { return this; } } }"#,
        "net/minecraft/world/level/block/BlockBehaviour.java" => r#"package net.minecraft.world.level.block; public class BlockBehaviour { protected BlockBehaviour(Properties p) { } public static class Properties { public Properties() { } public Properties strength(float hardness, float resistance) { return this; } } }"#,
        "net/minecraft/world/level/block/Block.java" => "package net.minecraft.world.level.block; public class Block extends BlockBehaviour { public Block(Properties p) { super(p); } }",
        "catalog/FabricCatalogMod.java" => r#"
package catalog;
import java.util.List;
import java.util.Optional;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.loader.api.FabricLoader;
import net.fabricmc.loader.api.ModContainer;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.BlockBehaviour;

public final class FabricCatalogMod implements ModInitializer {
  private static final class ReflectProbe {
    private static final String MARKER = "reflection-ok";
  }

  public void onInitialize() {
    Identifier ruby = Identifier.fromNamespaceAndPath("catalog", "ruby");
    Identifier lampId = Identifier.tryParse("catalog:ruby_lamp");
    Item.Properties itemProperties = new Item.Properties();
    itemProperties.stacksTo(64);
    Item rubyItem = new Item(itemProperties);
    BlockBehaviour.Properties blockProperties = new BlockBehaviour.Properties();
    blockProperties.strength(3.0F, 6.0F);
    Block lamp = new Block(blockProperties);
    Registry.register(Registries.ITEM, ruby, rubyItem);
    Registry.register(Registries.BLOCK, lampId, lamp);

    FabricLoader loader = FabricLoader.getInstance();
    Optional<ModContainer> self = loader.getModContainer("catalog-fabric");
    List<ModContainer> all = loader.getAllMods();
    if (self.isPresent() && !all.isEmpty()) {
      loader.isModLoaded("minecraft");
    }
    try {
      Class<?> probe = Class.forName("catalog.FabricCatalogMod$ReflectProbe");
      String marker = probe.getDeclaredField("MARKER").getName();
      loader.isModLoaded(marker);
    } catch (Exception failure) {
      throw new IllegalStateException(failure);
    }
  }
}
"#,
    };
    let (root, jar_path) = build_catalog_jar(
        "fabric-catalog",
        &stubs,
        &sources,
        "fabric.mod.json",
        br#"{"id":"catalog-fabric","version":"1.0","entrypoints":{"main":"catalog.FabricCatalogMod"}}"#,
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.fabric_id.as_deref(), Some("catalog-fabric"));
    let entry = jar.discover_entrypoint("main").unwrap();
    assert_eq!(entry.class, "catalog.FabricCatalogMod");

    let mut host = jar.seed_host(StdHost::new());
    let report = jar
        .execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();
    assert_eq!(report.method, "onInitialize");
    assert!(report.calls > 0, "the initializer must execute bytecode");

    // Identifier factories and the loader query surface.
    assert_called!(&host, IDENTIFIER, "fromNamespaceAndPath", 1);
    assert_called!(&host, IDENTIFIER, "tryParse", 1);
    assert_called!(&host, FABRIC_LOADER, "getInstance", 1);
    assert_called!(&host, FABRIC_LOADER, "getModContainer", 1);
    assert_called!(&host, FABRIC_LOADER, "getAllMods", 1);
    assert_called!(&host, FABRIC_LOADER, "isModLoaded", 2);

    // Registry ids keep their `namespace:path` shape and the registered values
    // arrive as the constructed content markers.  `args[0]` (the registry
    // handle from `Registries.ITEM`) is a GAP: registry statics are not resolved
    // by the host yet, so only the id and value slots are meaningful today.
    let registers = calls_to(&host, MC_REGISTRY, "register");
    assert_eq!(registers.len(), 2);
    assert_eq!(
        registers[0].args[1],
        Value::Ref(Some("identifier/catalog:ruby".into()))
    );
    assert_eq!(
        registers[1].args[1],
        Value::Ref(Some("identifier/catalog:ruby_lamp".into()))
    );
    for (register, expected) in registers.iter().zip(["Item#", "Block#"]) {
        assert!(
            matches!(register.args.get(2), Some(Value::Ref(Some(marker))) if marker.contains(expected)),
            "expected a {expected} content marker, got {:?}",
            register.args
        );
    }
    // `Registry.register` reaches the registry sink through the host's
    // registry/identifier markers, so the plain Fabric path records the same
    // typed lookup keys as the DeferredRegister paths.
    let registered: Vec<(&str, &str)> = host
        .registrations()
        .iter()
        .map(|entry| (entry.registry.as_str(), entry.id.as_str()))
        .collect();
    assert_eq!(
        registered,
        vec![
            ("minecraft:item", "catalog:ruby"),
            ("minecraft:block", "catalog:ruby_lamp")
        ],
        "plain Fabric Registry.register should reach the registry sink"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn neoforge_catalog_mod_registers_content_listener_and_payload() {
    let stubs = sources! {
        "net/neoforged/fml/common/Mod.java" => "package net.neoforged.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }",
        "net/neoforged/bus/api/IEventBus.java" => "package net.neoforged.bus.api; import java.util.function.Consumer; public interface IEventBus { default <T> IEventBus addListener(Consumer<T> listener) { return this; } }",
        "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext.java" => "package net.neoforged.fml.javafmlmod; import net.neoforged.bus.api.IEventBus; public final class FMLJavaModLoadingContext { public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); } public IEventBus getModEventBus() { return null; } }",
        "net/neoforged/neoforge/registries/DeferredHolder.java" => "package net.neoforged.neoforge.registries; public final class DeferredHolder<R, T> { }",
        "net/neoforged/neoforge/registries/DeferredRegister.java" => "package net.neoforged.neoforge.registries; import java.util.function.Supplier; import net.neoforged.bus.api.IEventBus; public final class DeferredRegister<T> { public static <T> DeferredRegister<T> create(Object registry, String namespace) { return null; } public void register(IEventBus bus) { } public <V extends T> DeferredHolder<T, V> register(String name, Supplier<V> value) { return null; } }",
        "net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent.java" => "package net.neoforged.neoforge.network.event; import net.neoforged.neoforge.network.registration.PayloadRegistrar; public final class RegisterPayloadHandlersEvent { public PayloadRegistrar registrar(String version) { return null; } }",
        "net/neoforged/neoforge/network/registration/PayloadRegistrar.java" => "package net.neoforged.neoforge.network.registration; import java.util.function.BiConsumer; public final class PayloadRegistrar { public PayloadRegistrar playToServer(Class<?> type, Object codec, BiConsumer<Object, Object> handler) { return this; } public PayloadRegistrar playToClient(Class<?> type, Object codec, BiConsumer<Object, Object> handler) { return this; } }",
    };
    let sources = sources! {
        "catalog/NeoCatalogMod.java" => r#"
package catalog;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext;
import net.neoforged.neoforge.network.event.RegisterPayloadHandlersEvent;
import net.neoforged.neoforge.network.registration.PayloadRegistrar;
import net.neoforged.neoforge.registries.DeferredHolder;
import net.neoforged.neoforge.registries.DeferredRegister;

@Mod("catalog-neo")
public final class NeoCatalogMod {
  private static final DeferredRegister<Object> ITEMS =
      DeferredRegister.create("minecraft:item", "catalog-neo");

  public NeoCatalogMod() {
    IEventBus bus = FMLJavaModLoadingContext.get().getModEventBus();
    ITEMS.register(bus);
    DeferredHolder<Object, Object> sapphire = ITEMS.register("sapphire", () -> new Object());
    bus.addListener(this::registerPayloads);
  }

  private void registerPayloads(RegisterPayloadHandlersEvent event) {
    PayloadRegistrar registrar = event.registrar("1");
    registrar.playToServer(Object.class, "catalog:codec", (payload, context) -> { });
    registrar.playToClient(Object.class, "catalog:client_codec", (payload, context) -> { });
  }
}
"#,
    };
    let (root, jar_path) = build_catalog_jar(
        "neoforge-catalog",
        &stubs,
        &sources,
        "META-INF/neoforge.mods.toml",
        br#"[[mods]]
modId = "catalog-neo"
version = "1.0"
"#,
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("catalog-neo"));
    let entry = jar.discover_entrypoint("neoforge").unwrap();
    assert_eq!(entry.class, "catalog.NeoCatalogMod");

    let mut host = jar.seed_host(StdHost::new());
    let report = jar
        .execute(&entry.class, entry.method.as_deref(), "neoforge", &mut host)
        .unwrap();
    assert_eq!(report.method, "<init>");
    assert!(report.calls > 0, "the @Mod constructor must execute");
    assert!(!host.invocations().is_empty());

    // DeferredRegister: `create` in <clinit>, one bulk register(IEventBus),
    // and one entry registration.  The source passes the registry id as a
    // string because `Registries.ITEM` is not a resolved value here (gap).
    // The registry component is the bounded
    // `deferred-register/<registry>/<namespace>` marker; the key keeps shape.
    assert_called!(&host, NEO_DEFERRED, "create", 1);
    assert_called!(&host, NEO_DEFERRED, "register", 2);
    assert_eq!(host.registrations().len(), 1);
    let registration = &host.registrations()[0];
    assert_eq!(registration.id, "sapphire");
    assert_eq!(
        registration.registry,
        "deferred-register/minecraft:item/catalog-neo"
    );
    assert_eq!(
        registration.key(),
        "deferred-register/minecraft:item/catalog-neo/sapphire"
    );
    assert!(matches!(registration.value, Value::Ref(Some(_))));
    assert!(registration.declaration.is_none(), "Java content is opaque");

    // The listener is recorded on the event bus and dispatched natively; the
    // payload registration happens inside the dispatched listener.
    assert_eq!(host.listener_registrations().count(), 1);
    let dispatches = host.dispatch_event(Value::Ref(Some("catalog:register_payloads".into())));
    assert_eq!(dispatches.len(), 1);
    assert_eq!(
        dispatches[0].listener.method.owner,
        "net/neoforged/bus/api/IEventBus"
    );
    jar.execute_dispatch(&dispatches[0], &mut host).unwrap();
    assert_called!(&host, NEO_PAYLOAD_EVENT, "registrar", 1);
    let payloads = host.payload_registrations();
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0].direction, "playToServer");
    assert_eq!(
        payloads[0].payload_type,
        Value::Ref(Some("java/lang/Object".into()))
    );
    assert_eq!(payloads[0].codec, Value::Ref(Some("catalog:codec".into())));
    assert!(matches!(payloads[0].handler, Value::Ref(Some(_))));
    assert_eq!(payloads[1].direction, "playToClient");
    assert_eq!(
        payloads[1].payload_type,
        Value::Ref(Some("java/lang/Object".into()))
    );
    assert_eq!(
        payloads[1].codec,
        Value::Ref(Some("catalog:client_codec".into()))
    );
    assert!(matches!(payloads[1].handler, Value::Ref(Some(_))));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn neoforge_catalog_mod_runs_direct_register_event_listener() {
    // NeoForge mods commonly register directly from RegisterEvent instead of
    // using DeferredRegister.  Keep this as a source-built real entrypoint so
    // the event-bus callback, method reference, supplier and registration all
    // cross the native host together.  The bounded RegisterEvent façade accepts
    // a nullable registry key here; this isolates the callback path while the
    // registry-key static identity remains a separately reported gap.
    let stubs = sources! {
        "net/neoforged/fml/common/Mod.java" => "package net.neoforged.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }",
        "net/neoforged/bus/api/IEventBus.java" => "package net.neoforged.bus.api; import java.util.function.Consumer; public interface IEventBus { default <T> IEventBus addListener(Consumer<T> listener) { return this; } }",
        "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext.java" => "package net.neoforged.fml.javafmlmod; import net.neoforged.bus.api.IEventBus; public final class FMLJavaModLoadingContext { public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); } public IEventBus getModEventBus() { return null; } }",
        "net/neoforged/neoforge/registries/RegisterEvent.java" => "package net.neoforged.neoforge.registries; import java.util.function.Supplier; import net.minecraft.resources.Identifier; import net.minecraft.resources.ResourceKey; public final class RegisterEvent { public void register(ResourceKey<?> key, Identifier id, Supplier<?> value) { } }",
        "net/minecraft/resources/Identifier.java" => "package net.minecraft.resources; public final class Identifier { public static Identifier fromNamespaceAndPath(String namespace, String path) { return null; } }",
        "net/minecraft/resources/ResourceKey.java" => "package net.minecraft.resources; public final class ResourceKey<T> { }",
    };
    let sources = sources! {
        "catalog/NeoRegisterEventMod.java" => r#"
package catalog;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext;
import net.neoforged.neoforge.registries.RegisterEvent;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.ResourceKey;

@Mod("catalog-register")
public final class NeoRegisterEventMod {
  public NeoRegisterEventMod() {
    IEventBus bus = FMLJavaModLoadingContext.get().getModEventBus();
    bus.addListener(this::register);
  }

  private void register(RegisterEvent event) {
    event.register(null, Identifier.fromNamespaceAndPath("catalog-register", "ruby"), () -> new Object());
  }
}
"#,
    };
    let (root, jar_path) = build_catalog_jar(
        "neoforge-register-event-catalog",
        &stubs,
        &sources,
        "META-INF/neoforge.mods.toml",
        br#"[[mods]]
modId = "catalog-register"
version = "1.0"
"#,
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    let entry = jar.discover_entrypoint("neoforge").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "neoforge", &mut host)
        .unwrap();

    assert_eq!(host.listener_registrations().count(), 1);
    let dispatches = host.dispatch_event(Value::Ref(Some("RegisterEvent".into())));
    assert_eq!(dispatches.len(), 1);
    jar.execute_dispatch(&dispatches[0], &mut host).unwrap();

    assert_called!(
        &host,
        "net/neoforged/neoforge/registries/RegisterEvent",
        "register",
        1
    );
    assert_eq!(host.registrations().len(), 1);
    let registration = &host.registrations()[0];
    assert_eq!(registration.registry, "minecraft:unknown");
    assert_eq!(registration.id, "catalog-register:ruby");
    assert!(matches!(registration.value, Value::Ref(Some(_))));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn legacy_forge_catalog_mod_registers_through_bounded_aliases() {
    let stubs = sources! {
        "net/minecraftforge/fml/common/Mod.java" => "package net.minecraftforge.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }",
        "net/minecraftforge/registries/RegistryObject.java" => "package net.minecraftforge.registries; public final class RegistryObject<T> { }",
        "net/minecraftforge/registries/DeferredRegister.java" => "package net.minecraftforge.registries; import java.util.function.Supplier; public final class DeferredRegister<T> { public static <T> DeferredRegister<T> create(Object registry, String namespace) { return null; } public <V extends T> RegistryObject<V> register(String name, Supplier<V> value) { return null; } }",
        "net/minecraftforge/network/simple/SimpleChannel.java" => "package net.minecraftforge.network.simple; public final class SimpleChannel { public SimpleChannel registerMessage(int id, Class<?> type, Object encoder, Object decoder, Object handler) { return this; } }",
    };
    let sources = sources! {
        "catalog/LegacyCatalogMod.java" => r#"
package catalog;
import net.minecraftforge.fml.common.Mod;
import net.minecraftforge.network.simple.SimpleChannel;
import net.minecraftforge.registries.DeferredRegister;
import net.minecraftforge.registries.RegistryObject;

@Mod("catalog-legacy")
public final class LegacyCatalogMod {
  private static final DeferredRegister<Object> ITEMS =
      DeferredRegister.create("item", "catalog-legacy");
  private final SimpleChannel channel = new SimpleChannel();

  public LegacyCatalogMod() {
    RegistryObject<Object> shard = ITEMS.register("emerald_shard", () -> new Object());
    channel.registerMessage(0, Object.class, null, null, null);
  }
}
"#,
    };
    let (root, jar_path) = build_catalog_jar(
        "legacy-forge-catalog",
        &stubs,
        &sources,
        "META-INF/mods.toml",
        br#"modLoader = "javafml"
[[mods]]
modId = "catalog-legacy"
version = "1.0"
"#,
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.neoforge_id.as_deref(), Some("catalog-legacy"));
    // Legacy `mods.toml` jars are discovered through the NeoForge alias.
    let entry = jar.discover_entrypoint("neoforge").unwrap();
    assert_eq!(entry.class, "catalog.LegacyCatalogMod");

    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "neoforge", &mut host)
        .unwrap();

    assert_called!(&host, FORGE_DEFERRED, "create", 1);
    assert_eq!(host.registrations().len(), 1);
    assert_eq!(
        host.registrations()[0].key(),
        "deferred-register/item/catalog-legacy/emerald_shard"
    );
    assert!(matches!(host.registrations()[0].value, Value::Ref(Some(_))));

    // SimpleChannel is carried as a bounded bidirectional payload.
    assert_called!(&host, FORGE_CHANNEL, "registerMessage", 1);
    let payloads = host.payload_registrations();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].direction, "playBidirectional");
    assert_eq!(
        payloads[0].payload_type,
        Value::Ref(Some("java/lang/Object".into()))
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn representative_loader_mods_register_and_receive_server_tick_events() {
    // Keep this fixture deliberately close to the bootstrap code in small
    // production mods: Fabric uses an array-backed event, while Forge and
    // NeoForge attach a method reference to their mod event bus.  The loader
    // classes live only on the compile classpath, so the jar exercises the
    // same host boundary as an installed mod.
    let fabric_stubs = sources! {
        "net/fabricmc/api/ModInitializer.java" =>
            "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
        "net/fabricmc/fabric/api/event/Event.java" =>
            "package net.fabricmc.fabric.api.event; public interface Event<T> { void register(T listener); }",
        "net/fabricmc/fabric/api/event/EventFactory.java" =>
            "package net.fabricmc.fabric.api.event; import java.util.function.Function; public final class EventFactory { public static <T> Event<T> createArrayBacked(Class<T> type, Function<T[], T> factory) { return null; } }",
        "net/fabricmc/fabric/api/event/lifecycle/v1/ServerTickEvents.java" => r#"
package net.fabricmc.fabric.api.event.lifecycle.v1;
import net.fabricmc.fabric.api.event.Event;
import net.fabricmc.fabric.api.event.EventFactory;
import net.minecraft.server.MinecraftServer;
public final class ServerTickEvents {
  @FunctionalInterface public interface Start { void onStartTick(MinecraftServer server); }
  public static final Event<Start> START_SERVER_TICK = EventFactory.createArrayBacked(
      Start.class, listeners -> server -> { for (Start listener : listeners) listener.onStartTick(server); });
}
"#,
        "net/minecraft/server/MinecraftServer.java" =>
            "package net.minecraft.server; public final class MinecraftServer { }",
    };
    let fabric_sources = sources! {
        "catalog/FabricTickMod.java" => r#"
package catalog;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerTickEvents;
import net.minecraft.server.MinecraftServer;
public final class FabricTickMod implements ModInitializer {
  public void onInitialize() { ServerTickEvents.START_SERVER_TICK.register(this::onTick); }
  private void onTick(MinecraftServer server) { System.getenv("fabric-server-tick"); }
}
"#,
    };
    let (fabric_root, fabric_path) = build_catalog_jar(
        "fabric-server-tick",
        &fabric_stubs,
        &fabric_sources,
        "fabric.mod.json",
        br#"{"id":"fixture-fabric-tick","version":"1.0","entrypoints":{"main":"catalog.FabricTickMod"}}"#,
    );

    let forge_stubs = sources! {
        "net/minecraftforge/fml/common/Mod.java" => "package net.minecraftforge.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }",
        "net/minecraftforge/eventbus/api/IEventBus.java" =>
            "package net.minecraftforge.eventbus.api; import java.util.function.Consumer; public interface IEventBus { default <T> IEventBus addListener(Consumer<T> listener) { return this; } }",
        "net/minecraftforge/fml/javafmlmod/FMLJavaModLoadingContext.java" => "package net.minecraftforge.fml.javafmlmod; import net.minecraftforge.eventbus.api.IEventBus; public final class FMLJavaModLoadingContext { public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); } public IEventBus getModEventBus() { return null; } }",
        "net/minecraftforge/event/TickEvent.java" => r#"
package net.minecraftforge.event;
public final class TickEvent { public static final class ServerTickEvent { } }
"#,
    };
    let forge_sources = sources! {
        "catalog/ForgeTickMod.java" => r#"
package catalog;
import net.minecraftforge.event.TickEvent;
import net.minecraftforge.eventbus.api.IEventBus;
import net.minecraftforge.fml.common.Mod;
import net.minecraftforge.fml.javafmlmod.FMLJavaModLoadingContext;
@Mod("fixture-forge-tick") public final class ForgeTickMod {
  public ForgeTickMod() {
    IEventBus bus = FMLJavaModLoadingContext.get().getModEventBus();
    bus.addListener(this::onTick);
  }
  private void onTick(TickEvent.ServerTickEvent event) { System.getenv("forge-server-tick"); }
}
"#,
    };
    let (forge_root, forge_path) = build_catalog_jar(
        "forge-server-tick",
        &forge_stubs,
        &forge_sources,
        "META-INF/mods.toml",
        br#"modLoader = "javafml"
[[mods]]
modId = "fixture-forge-tick"
version = "1.0"
"#,
    );

    let neo_stubs = sources! {
        "net/neoforged/fml/common/Mod.java" => "package net.neoforged.fml.common; import java.lang.annotation.*; @Retention(RetentionPolicy.RUNTIME) public @interface Mod { String value(); }",
        "net/neoforged/bus/api/IEventBus.java" =>
            "package net.neoforged.bus.api; import java.util.function.Consumer; public interface IEventBus { default <T> IEventBus addListener(Consumer<T> listener) { return this; } }",
        "net/neoforged/fml/javafmlmod/FMLJavaModLoadingContext.java" => "package net.neoforged.fml.javafmlmod; import net.neoforged.bus.api.IEventBus; public final class FMLJavaModLoadingContext { public static FMLJavaModLoadingContext get() { return new FMLJavaModLoadingContext(); } public IEventBus getModEventBus() { return null; } }",
        "net/neoforged/neoforge/event/tick/ServerTickEvent.java" => r#"
package net.neoforged.neoforge.event.tick;
public final class ServerTickEvent { public static final class Pre { } }
"#,
    };
    let neo_sources = sources! {
        "catalog/NeoTickMod.java" => r#"
package catalog;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.fml.common.Mod;
import net.neoforged.fml.javafmlmod.FMLJavaModLoadingContext;
import net.neoforged.neoforge.event.tick.ServerTickEvent;
@Mod("fixture-neo-tick") public final class NeoTickMod {
  public NeoTickMod() {
    IEventBus bus = FMLJavaModLoadingContext.get().getModEventBus();
    bus.addListener(this::onTick);
  }
  private void onTick(ServerTickEvent.Pre event) { System.getenv("neo-server-tick"); }
}
"#,
    };
    let (neo_root, neo_path) = build_catalog_jar(
        "neoforge-server-tick",
        &neo_stubs,
        &neo_sources,
        "META-INF/neoforge.mods.toml",
        br#"[[mods]]
modId = "fixture-neo-tick"
version = "1.0"
"#,
    );

    let cases = [
        (fabric_path, "main", "fabric-server-tick"),
        (forge_path, "neoforge", "forge-server-tick"),
        (neo_path, "neoforge", "neo-server-tick"),
    ];
    for (path, kind, marker) in cases {
        let jar = NativeJar::from_path(&path).unwrap();
        let entry = jar.discover_entrypoint(kind).unwrap();
        let mut host = jar.seed_host(StdHost::new());
        jar.execute(&entry.class, entry.method.as_deref(), kind, &mut host)
            .unwrap();
        let dispatches = host.dispatch_event(Value::Ref(Some("ServerTickEvent.Pre".into())));
        assert_eq!(dispatches.len(), 1, "{kind} listener should be retained");
        jar.execute_dispatch(&dispatches[0], &mut host).unwrap();
        // The server loop uses the same resident host for the matching Post
        // phase. Keep this explicit so a fixture cannot accidentally pass
        // while only the pre tick marker is dispatchable.
        let post_dispatches = host.dispatch_event(Value::Ref(Some("ServerTickEvent.Post".into())));
        assert_eq!(
            post_dispatches.len(),
            1,
            "{kind} listener should receive post ticks"
        );
        jar.execute_dispatch(&post_dispatches[0], &mut host)
            .unwrap();
        let getenv = host
            .invocations()
            .iter()
            .filter(|call| call.method.owner == "java/lang/System" && call.method.name == "getenv")
            .map(|call| call.args.first().cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            getenv,
            vec![
                Some(Value::Ref(Some(marker.into()))),
                Some(Value::Ref(Some(marker.into()))),
            ],
            "{kind} callback did not run"
        );
    }

    let _ = fs::remove_dir_all(fabric_root);
    let _ = fs::remove_dir_all(forge_root);
    let _ = fs::remove_dir_all(neo_root);
}

#[test]
fn fabric_entrypoint_without_on_initialize_reports_missing_method() {
    let sources = sources! {
        "catalog/BrokenMod.java" => "package catalog; public final class BrokenMod { public void somethingElse() { } }",
    };
    let (root, jar_path) = build_catalog_jar(
        "fabric-broken",
        &BTreeMap::new(),
        &sources,
        "fabric.mod.json",
        br#"{"id":"catalog-broken","version":"1.0","entrypoints":{"main":"catalog.BrokenMod"}}"#,
    );

    let jar = NativeJar::from_path(&jar_path).unwrap();
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    let error = jar
        .execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .expect_err("a class without onInitialize() must fail explicitly");

    assert!(
        matches!(error, NativeError::MissingMethod(_)),
        "expected MissingMethod, got {error:?}"
    );
    let text = error.to_string();
    assert!(
        text.contains("catalog/BrokenMod") && text.contains("onInitialize"),
        "the error must name the class and method: {text}"
    );

    let _ = fs::remove_dir_all(root);
}

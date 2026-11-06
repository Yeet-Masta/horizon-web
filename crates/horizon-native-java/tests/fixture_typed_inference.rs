//! A source-built Fabric jar registers an item and a block the ordinary way,
//! with no `META-INF/horizon-native.json`.  The host sees the property-builder
//! calls the stub classes receive, so `Registry.register` can infer a bounded
//! [`horizon_native_java::RegistryDeclaration`] for a value it would otherwise
//! record as opaque.
//!
//! The stubs are compiled onto a separate classpath and deliberately *not*
//! packaged in the jar, mirroring a real install where the game is a host
//! surface: every builder call crosses `Host` and is observable on `StdHost`.

use horizon_native_java::{Host, MethodRef, NativeJar, RegistryDeclaration, StdHost, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use zip::write::SimpleFileOptions;

const ITEM_PROPERTIES: &str = "net/minecraft/world/item/Item$Properties";
const ITEM: &str = "net/minecraft/world/item/Item";
const REGISTER: &str = "(Lnet/minecraft/core/Registry;Lnet/minecraft/resources/Identifier;\
                       Ljava/lang/Object;)Ljava/lang/Object;";

/// A Java source tree for [`build_jar`]: relative path => source text.
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
        "horizon-native-java-typed-inference-{name}-{}-{}",
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

/// Compile API stubs onto a classpath, compile the mod against them, and
/// package only the mod's own class files plus its loader metadata.
fn build_jar(
    name: &str,
    stubs: &BTreeMap<&str, &str>,
    sources: &BTreeMap<&str, &str>,
    metadata: &[u8],
    native: Option<&[u8]>,
) -> (PathBuf, PathBuf) {
    let root = scratch_root(name);
    let stub_classes = root.join("stub-classes");
    let classes = root.join("classes");
    let stub_paths = write_sources(&root.join("stub-src"), stubs);
    compile(&stub_classes, None, &stub_paths, name);
    let paths = write_sources(&root.join("src"), sources);
    compile(&classes, Some(&stub_classes), &paths, name);

    let jar_path = root.join(format!("{name}.jar"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    zip.start_file("fabric.mod.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(metadata).unwrap();
    if let Some(native) = native {
        zip.start_file("META-INF/horizon-native.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(native).unwrap();
    }
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

/// The game surface a small Fabric content mod compiles against: the two
/// property builders, the classes they build, the registry and its handles.
fn game_stubs() -> BTreeMap<&'static str, &'static str> {
    sources! {
        "net/fabricmc/api/ModInitializer.java" =>
            "package net.fabricmc.api; public interface ModInitializer { void onInitialize(); }",
        "net/minecraft/resources/Identifier.java" => r#"package net.minecraft.resources;
public final class Identifier {
    private Identifier() { }
    public static Identifier fromNamespaceAndPath(String namespace, String path) { return null; }
}"#,
        "net/minecraft/world/item/Rarity.java" => r#"package net.minecraft.world.item;
    public enum Rarity { COMMON, UNCOMMON, RARE, EPIC }"#,
        "net/minecraft/world/item/Item.java" => r#"package net.minecraft.world.item;
public class Item {
    public Item(Properties properties) { }
    public static class Properties {
        public Properties() { }
        public Properties stacksTo(int count) { return this; }
        public Properties durability(int maxDamage) { return this; }
        public Properties rarity(Rarity rarity) { return this; }
        public Properties fireResistant() { return this; }
        public Properties craftingRemainingItem(Item item) { return this; }
    }
}"#,
        "net/minecraft/world/level/block/state/BlockBehaviour.java" => r#"package net.minecraft.world.level.block.state;
public class BlockBehaviour {
    public static class Properties {
        public Properties() { }
        public static Properties of() { return new Properties(); }
        public Properties strength(float destroyTime) { return this; }
        public Properties strength(float destroyTime, float explosionResistance) { return this; }
        public Properties friction(float friction) { return this; }
        public Properties speedFactor(float factor) { return this; }
        public Properties jumpFactor(float factor) { return this; }
        public Properties randomTicks() { return this; }
        public Properties noOcclusion() { return this; }
        public Properties requiresCorrectToolForDrops() { return this; }
        public Properties instabreak() { return this; }
    }
}"#,
        "net/minecraft/world/level/block/Block.java" => r#"package net.minecraft.world.level.block;
import net.minecraft.world.level.block.state.BlockBehaviour;
public class Block {
    public Block(BlockBehaviour.Properties properties) { }
}"#,
        "net/minecraft/core/Registry.java" => r#"package net.minecraft.core;
import net.minecraft.resources.Identifier;
public interface Registry<T> {
    static <V, T extends V> T register(Registry<V> registry, Identifier id, T value) {
        return value;
    }
}"#,
        "net/minecraft/core/registries/Registries.java" => r#"package net.minecraft.core.registries;
import net.minecraft.core.Registry;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.block.Block;
public final class Registries {
    public static final Registry<Item> ITEM = null;
    public static final Registry<Block> BLOCK = null;
    public static final Registry<Object> SOUND_EVENT = null;
}"#,
    }
}

/// The mod: an item built from a chained `Item.Properties`, a block built from
/// a chained `BlockBehaviour.Properties`, and one registration in a registry
/// the host has no template for.
fn typed_mod() -> BTreeMap<&'static str, &'static str> {
    sources! {
        "demo/TypedInferenceMod.java" => r#"
package demo;
import net.fabricmc.api.ModInitializer;
import net.minecraft.core.Registry;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.Identifier;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.Rarity;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockBehaviour;

public final class TypedInferenceMod implements ModInitializer {
    @Override
    public void onInitialize() {
        // 16 per stack, 32 durability, rare: only max_stack_size and rarity
        // have a content-loader key, so durability stays in the host record.
        Item wand = new Item(new Item.Properties().stacksTo(16).durability(32).rarity(Rarity.RARE));
        Registry.register(Registries.ITEM, Identifier.fromNamespaceAndPath("fixture", "wand"), wand);

        Block lamp = new Block(BlockBehaviour.Properties.of()
                .strength(2.0F, 6.0F).requiresCorrectToolForDrops());
        Registry.register(Registries.BLOCK, Identifier.fromNamespaceAndPath("fixture", "lamp"), lamp);

        // No template vocabulary for a sound event: the row must stay opaque.
        Registry.register(Registries.SOUND_EVENT,
                Identifier.fromNamespaceAndPath("fixture", "chime"),
                new Item(new Item.Properties()));
    }
}
"#,
    }
}

fn construct(
    host: &mut StdHost,
    owner: &str,
    marker: &str,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> Value {
    host.invoke_instance(
        &MethodRef {
            owner: owner.into(),
            name: name.into(),
            descriptor: descriptor.into(),
        },
        &Value::Ref(Some(marker.into())),
        args,
    )
    .unwrap()
}

#[test]
fn fabric_fixture_infers_typed_item_and_block_declarations() {
    let (root, jar_path) = build_jar(
        "typed-inference",
        &game_stubs(),
        &typed_mod(),
        br#"{"id":"fixture-typed","version":"1.0","entrypoints":{"main":"demo.TypedInferenceMod"}}"#,
        None,
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert!(
        jar.native_declarations.is_empty(),
        "no metadata declarations"
    );
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();

    let keys = host
        .registrations()
        .iter()
        .map(|entry| entry.key())
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "minecraft:item/fixture:wand",
            "minecraft:block/fixture:lamp",
            "minecraft:sound_event/fixture:chime",
        ]
    );

    let wand = &host.registrations()[0];
    assert_eq!(
        wand.declaration,
        Some(RegistryDeclaration::Item {
            template: "minecraft:stick".into(),
            properties: BTreeMap::from([
                ("max_stack_size".into(), "16".into()),
                ("rarity".into(), "rare".into()),
            ]),
        })
    );
    // The full chain is kept: `durability` has no loader key, so it is not
    // exported and cannot fail the declaration for an unknown property.
    let Value::Ref(Some(wand_marker)) = &wand.value else {
        panic!("item value is not a marker: {:?}", wand.value);
    };
    assert_eq!(
        host.item_properties(wand_marker),
        Some(&BTreeMap::from([
            ("durability".into(), "32".into()),
            ("rarity".into(), "rare".into()),
            ("stacksTo".into(), "16".into()),
        ]))
    );

    let lamp = &host.registrations()[1];
    assert_eq!(
        lamp.declaration,
        Some(RegistryDeclaration::Block {
            template: "minecraft:stone".into(),
            properties: BTreeMap::from([
                ("explosion_resistance".into(), "6".into()),
                ("hardness".into(), "2".into()),
                ("requires_tool".into(), "true".into()),
            ]),
            item: true,
        })
    );
    let Value::Ref(Some(lamp_marker)) = &lamp.value else {
        panic!("block value is not a marker: {:?}", lamp.value);
    };
    assert_eq!(
        host.block_properties(lamp_marker),
        Some(&BTreeMap::from([
            ("destroyTime".into(), "2".into()),
            ("explosionResistance".into(), "6".into()),
            ("requiresCorrectToolForDrops".into(), "true".into()),
        ]))
    );

    // The host has no item or block template for a sound event, so the row it
    // saw a builder for still stays opaque rather than inventing content.
    assert_eq!(host.registrations()[2].declaration, None);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_declaration_replaces_an_inferred_one() {
    let mut host = StdHost::new();
    construct(
        &mut host,
        ITEM_PROPERTIES,
        "net/minecraft/world/item/Item$Properties#0",
        "<init>",
        "()V",
        &[],
    );
    construct(
        &mut host,
        ITEM_PROPERTIES,
        "net/minecraft/world/item/Item$Properties#0",
        "stacksTo",
        "(I)Lnet/minecraft/world/item/Item$Properties;",
        &[Value::Int(16)],
    );
    construct(
        &mut host,
        ITEM,
        "net/minecraft/world/item/Item#0",
        "<init>",
        "(Lnet/minecraft/world/item/Item$Properties;)V",
        &[Value::Ref(Some(
            "net/minecraft/world/item/Item$Properties#0".into(),
        ))],
    );
    let register = MethodRef {
        owner: "net/minecraft/core/Registry".into(),
        name: "register".into(),
        descriptor: REGISTER.into(),
    };
    host.invoke(
        &register,
        &[
            Value::Ref(Some("registry/minecraft:item".into())),
            Value::Ref(Some("identifier/ruby:wand".into())),
            Value::Ref(Some("net/minecraft/world/item/Item#0".into())),
        ],
    )
    .unwrap();
    assert_eq!(
        host.registrations()[0].declaration,
        Some(RegistryDeclaration::Item {
            template: "minecraft:stick".into(),
            properties: BTreeMap::from([("max_stack_size".into(), "16".into())]),
        })
    );

    // An explicit adapter (metadata or a loader row) stays authoritative over
    // what the host inferred from the builder chain.
    let explicit = RegistryDeclaration::Item {
        template: "minecraft:stick".into(),
        properties: BTreeMap::new(),
    };
    host.attach_registration_declaration("minecraft:item", "ruby:wand", explicit.clone())
        .unwrap();
    assert_eq!(host.registrations()[0].declaration, Some(explicit.clone()));

    // An opaque registration keeps the all-or-nothing path: the first explicit
    // declaration lands, a second one cannot silently replace it.
    host.invoke(
        &register,
        &[
            Value::Ref(Some("registry/minecraft:item".into())),
            Value::Ref(Some("identifier/ruby:plain".into())),
            Value::Ref(Some("@indy:plain-item".into())),
        ],
    )
    .unwrap();
    host.attach_registration_declaration("minecraft:item", "ruby:plain", explicit.clone())
        .unwrap();
    let error = host
        .attach_registration_declaration("minecraft:item", "ruby:plain", explicit)
        .unwrap_err();
    assert!(
        error.contains("already has a declaration"),
        "unexpected error: {error}"
    );
}

#[test]
fn inference_records_a_plain_item_with_no_properties() {
    let mut host = StdHost::new();
    let properties = "net/minecraft/world/item/Item$Properties#0";
    construct(&mut host, ITEM_PROPERTIES, properties, "<init>", "()V", &[]);
    construct(
        &mut host,
        ITEM,
        "net/minecraft/world/item/Item#0",
        "<init>",
        "(Lnet/minecraft/world/item/Item$Properties;)V",
        &[Value::Ref(Some(properties.into()))],
    );
    host.invoke(
        &MethodRef {
            owner: "net/minecraft/core/Registry".into(),
            name: "register".into(),
            descriptor: REGISTER.into(),
        },
        &[
            Value::Ref(Some("registry/minecraft:item".into())),
            Value::Ref(Some("identifier/ruby:plain".into())),
            Value::Ref(Some("net/minecraft/world/item/Item#0".into())),
        ],
    )
    .unwrap();

    // A builder with no settings is still a declaration: the mod said "an
    // item", and its empty chain is not the same as an opaque marker.
    assert_eq!(
        host.registrations()[0].declaration,
        Some(RegistryDeclaration::Item {
            template: "minecraft:stick".into(),
            properties: BTreeMap::new(),
        })
    );
    // A second builder keeps its own chain, and a marker no `new` site or
    // factory named stays unknown.
    let second = "net/minecraft/world/item/Item$Properties#1";
    construct(&mut host, ITEM_PROPERTIES, second, "<init>", "()V", &[]);
    construct(
        &mut host,
        ITEM_PROPERTIES,
        second,
        "stacksTo",
        "(I)Lnet/minecraft/world/item/Item$Properties;",
        &[Value::Int(1)],
    );
    assert_eq!(host.item_properties(properties), Some(&BTreeMap::new()));
    assert_eq!(
        host.item_properties(second),
        Some(&BTreeMap::from([("stacksTo".into(), "1".into())]))
    );
    assert_eq!(
        host.item_properties("net/minecraft/world/item/Item$Properties#9"),
        None
    );
}

#[test]
fn metadata_declaration_keeps_that_row_opaque_to_inference() {
    let (root, jar_path) = build_jar(
        "typed-inference-metadata",
        &game_stubs(),
        &typed_mod(),
        br#"{"id":"fixture-typed","version":"1.0","entrypoints":{"main":"demo.TypedInferenceMod"}}"#,
        Some(
            br#"{"id":"fixture-typed","registrations":[{"registry":"minecraft:item","id":"fixture:wand","kind":"item","template":"minecraft:stick"}]}"#,
        ),
    );
    let jar = NativeJar::from_path(&jar_path).unwrap();
    assert_eq!(jar.native_declarations.len(), 1);
    let entry = jar.discover_entrypoint("main").unwrap();
    let mut host = jar.seed_host(StdHost::new());
    jar.execute(&entry.class, entry.method.as_deref(), "main", &mut host)
        .unwrap();

    let row = |host: &StdHost, id: &str| {
        host.registrations()
            .iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("{id} was not recorded"))
            .declaration
            .clone()
    };
    // The jar's own metadata is the explicit adapter for `fixture:wand`, so
    // the host leaves it opaque and the metadata merge stays authoritative.
    assert_eq!(row(&host, "fixture:wand"), None);
    // The block has no metadata row, so inference still supplies one.
    assert!(matches!(
        row(&host, "fixture:lamp"),
        Some(RegistryDeclaration::Block { .. })
    ));

    let _ = fs::remove_dir_all(root);
}
